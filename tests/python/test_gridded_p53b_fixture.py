"""P53b's committed Transverse Mercator fixture: provenance, and one case through the Python API.

`crates/dss-vortex/tests/tm_fixture.rs` runs every case against Vortex's recorded output. This holds the
files to the hashes `fixtures/public/README.md` lists, and shows the default and Vortex cell-size rules
from Python.
"""
import hashlib
import json
from pathlib import Path

import numpy as np
import pytest

import ras_commander_fast_dss as dss
from ras_commander_fast_dss import exceptions, gridded

FIXTURES = Path(__file__).resolve().parents[2] / "fixtures" / "public"
P53B = FIXTURES / "vortex_p53b"


def test_every_fixture_file_has_the_hash_the_readme_lists():
    readme = (FIXTURES / "README.md").read_text(encoding="utf-8")
    listed = {}
    for line in readme.splitlines():
        cells = [c.strip(" `") for c in line.split("|")]
        if len(cells) > 5 and cells[1].startswith("vortex_p53b/"):
            listed[cells[1].split("/", 1)[1]] = cells[5]
    on_disk = {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in P53B.iterdir()}
    assert len(on_disk) == 30 and listed == on_disk
    # The JSON is committed with LF endings (`-text`), or its hash would differ by checkout.
    for name in ("expected.json", "expected_tools.json"):
        assert bytes([13, 10]) not in (P53B / name).read_bytes()


def case(name):
    cases = json.loads((P53B / "expected.json").read_text(encoding="utf-8"))["cases"]
    return next(c for c in cases if c["id"] == name)


def run(c, out, **kw):
    e = c["envelope"]
    args = dict(variable=c["variable"], method=c["method"], target_crs=c["target"], cell_size=c["cell"],
                envelope=(e[0], e[2], e[1], e[3]), envelope_crs=c["envelope_wkt"], parts={"A": "TM", "B": "P53B", "F": "CI"})
    args.update(kw)
    return gridded.import_gridded([P53B / i for i in c["inputs"]], out, **args)


def test_a_cell_size_in_metres_on_a_us_survey_foot_target_follows_align(tmp_path):
    c = case("target.ftus.vortex_rule")
    # Vortex's rule: 6096 m is 20,000 international feet, also on this US survey foot zone.
    out = tmp_path / "vortex.dss"
    result = run(c, out, align="vortex")
    with dss.open(out) as f:
        grid = f.read_grid(result.pathnames[0])
    assert (grid.cell_size, grid.data.shape) == (20000.0, (5, 8))
    assert result.records[0]["cell_size_requested_metres"] == 6096.0
    assert abs(result.records[0]["cell_size_stored_metres"] - 20000.0 * 1200 / 3937) < 1e-9
    # The default: the zone's own foot, at the spacing a record stores. 6096 m is 19,999.96 ft, and the
    # same envelope holds 9 by 6 of those cells, not Vortex's 8 by 5.
    default = tmp_path / "default.dss"
    result = run(c, default)
    with dss.open(default) as f:
        grid = f.read_grid(result.pathnames[0])
    assert (grid.cell_size, grid.data.shape) == (np.float32(19999.96), (6, 9))
    # The result states what was stored and how far that is from the request: here 0.286 mm over.
    r = result.records[0]
    assert r["cell_size_requested_metres"] == 6096.0
    assert abs(r["cell_size_stored_metres"] - 6096.000285750571) < 1e-9
    assert r["cell_size_difference_metres"] == r["cell_size_stored_metres"] - 6096.0
    # 2 km: written, 49.6 micrometres short of the request.
    two = tmp_path / "two.dss"
    result = run(c, two, cell_size=2000)
    r = result.records[0]
    assert r["cell_size"] == np.float32(6561.6665)
    assert abs(r["cell_size_difference_metres"] + 4.96e-5) < 1e-6
    # 1200 m is 3,937 US survey feet exactly: the default writes the spacing asked for.
    own = tmp_path / "own.dss"
    result = run(c, own, cell_size=1200, envelope=(3937.0 * 585, 3937.0 * 498, 3937.0 * 625, 3937.0 * 523))
    with dss.open(own) as f:
        grid = f.read_grid(result.pathnames[0])
    assert (grid.cell_size, grid.data.shape) == (3937.0, (25, 40))
    assert result.records[0]["cell_size_difference_metres"] == 0.0
    assert 3937.0 * 1200 / 3937 == 1200.0


def test_a_raster_off_whole_cells_is_refused_unwarped_by_default(tmp_path):
    c = case("source.ftus_offset.asis.snapped")
    out = tmp_path / "out.dss"
    kw = dict(variable=c["variable"], target_crs=None, cell_size=None, parts={"A": "TM", "B": "P53B", "C": "RASTER", "F": "CI"})
    with pytest.raises(exceptions.DssUnsupportedRecordError, match="whole cells"):
        gridded.import_gridded([P53B / c["inputs"][0]], out, **kw)
    assert not out.exists()
    # Asked for, Vortex's placement: the grid moves to whole cells, 736 ft west and 589 ft south.
    result = gridded.import_gridded([P53B / c["inputs"][0]], out, align="vortex", **kw)
    assert len(result.pathnames) == 1

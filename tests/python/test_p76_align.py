"""P76: ``align="snap_out"`` and the ``alignment`` note of gridded import records.

A geographic grid warped to SHG at 2 km with no envelope uses GDAL's suggested extent, whose
corner is off whole cells. The default refuses it (unchanged); ``"snap_out"`` snaps the extent
outward and stores each value where it was sampled (no shift); ``"vortex"`` stores Vortex's
record and reports the corner shift and the uncovered cells. Synthetic arrays only.
"""

from __future__ import annotations

import datetime as dt

import numpy as np
import pytest

import ras_commander_fast_dss as dss
from ras_commander_fast_dss import gridded

NAD83 = ('GEOGCS["NAD83",DATUM["North_American_Datum_1983",SPHEROID["GRS 1980",6378137,'
         '298.257222101]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]')
T = dt.datetime(2024, 6, 14, 23)


def run(tmp_path, align, name):
    data = (np.arange(90 * 120, dtype=np.float32).reshape(1, 90, 120) % 17) * 0.5
    return gridded.import_arrays(
        data, tmp_path / name, times=[(T, T + dt.timedelta(hours=1))],
        transform=(-90.0123, 0.01, 35.0071, -0.01), crs=NAD83, variable="APCP_surface",
        source_units="kg/m^2", target_crs="SHG", cell_size=2000.0, align=align,
        parts={"A": "SHG", "B": "T", "F": "P76"},
    )


def test_default_is_unchanged(tmp_path):
    with pytest.raises(Exception, match="not on whole cells"):
        run(tmp_path, "refuse", "r.dss")


def test_snap_out_and_vortex_report_their_alignment(tmp_path):
    snap = run(tmp_path, "snap_out", "s.dss").records[0]
    vortex = run(tmp_path, "vortex", "v.dss").records[0]
    a, b = snap["alignment"], vortex["alignment"]
    # snap_out: sampled on the stored lattice, no shift.
    assert a["shift_cells"] == (0.0, 0.0) and a["shift_map_units"] == (0.0, 0.0)
    assert a["stored_lower_left"] == a["sampled_lower_left"]
    assert a["sampled_transform"][1] == 2000.0 and a["stored_cell_size"] == 2000.0
    # vortex: sampled on GDAL's lattice, declared corner rounded.
    assert all(abs(v) <= 0.5 for v in b["shift_cells"]) and b["shift_cells"] != (0.0, 0.0)
    assert b["shift_map_units"] == pytest.approx(tuple(2000.0 * v for v in b["shift_cells"]))
    assert b["sampled_lower_left"][0] == b["sampled_transform"][0]
    assert snap["nx"] * snap["ny"] >= vortex["nx"] * vortex["ny"]
    assert snap["lower_left_cell_x"] * 2000.0 == a["stored_lower_left"][0]
    # missing_cells is the data's count; uncovered_centres is geometry of centres.
    for name, rec in (("s.dss", snap), ("v.dss", vortex)):
        with dss.open(tmp_path / name) as f:
            g = f.read_grid(rec["pathname"])
        assert int(np.isnan(g.data).sum()) == rec["alignment"]["missing_cells"]
        assert isinstance(rec["alignment"]["uncovered_centres"], int)
    assert a["missing_cells"] > 0 and b["missing_cells"] > 0


def test_unwarped_off_grid_is_refused_under_snap_out(tmp_path):
    data = np.ones((1, 3, 4), dtype=np.float32)
    kw = dict(times=[(T, T + dt.timedelta(hours=1))], transform=(2500.0, 2000.0, 20000.0, -2000.0),
              crs="SHG", variable="APCP_surface", source_units="kg/m^2")
    with pytest.raises(Exception, match="not on whole cells"):
        gridded.import_arrays(data, tmp_path / "u.dss", align="snap_out", **kw)
    r = gridded.import_arrays(data, tmp_path / "v.dss", align="vortex", **kw).records[0]
    assert r["alignment"]["shift_cells"] == (-0.25, 0.0)
    assert r["alignment"]["shift_map_units"] == (-500.0, 0.0)
    assert r["alignment"]["missing_cells"] == 0
    assert r["alignment"]["uncovered_centres"] is None


def test_alignment_is_none_by_default(tmp_path):
    data = np.ones((1, 3, 4), dtype=np.float32)
    r = gridded.import_arrays(data, tmp_path / "d.dss", times=[(T, T + dt.timedelta(hours=1))],
                              transform=(2000.0, 2000.0, 8000.0, -2000.0), crs="SHG",
                              variable="APCP_surface", source_units="kg/m^2").records[0]
    assert r["alignment"] is None


def test_bad_align_names_the_choices(tmp_path):
    with pytest.raises(ValueError, match='"refuse", "snap_out" or "vortex"'):
        run(tmp_path, "outward", "x.dss")

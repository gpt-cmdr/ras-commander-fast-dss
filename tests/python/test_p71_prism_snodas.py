"""P71: PRISM BIL and zipped rasters, and SNODAS, through `gridded.import_gridded`.

Real public files from `fixtures/public/p71` (see the fixtures README). Reader parity with Vortex
1.1.0 and 0.13.3 is `crates/dss-vortex/tests/p71_sources.rs`.
"""

from __future__ import annotations

import shutil

import numpy as np
import pytest

import ras_commander_fast_dss as dss
from ras_commander_fast_dss import gridded

from conftest import fixture_path

STEM = "us_ssmv11039lL00T0024TTNATS2020010105DP000"
PRISM = "p71/PRISM_ppt_stable_4kmD1_19990101_bil.zip"
SNOW = "Sublimation of Blowing Snow"


def test_features():
    assert {"gridded_prism_bil", "gridded_snodas"} <= dss.FEATURES


def test_prism_zip_imports_as_vortex_names_it(tmp_path):
    src = fixture_path(PRISM)
    assert gridded.list_variables(src) == ["precipitation"]
    out = tmp_path / "prism.dss"
    result = gridded.import_gridded([src], out, variable="precipitation", cell_size=2000.0, align="vortex")
    [path] = result.pathnames
    parts = path.split("/")
    assert parts[3:6] == ["PRECIPITATION", "31DEC1998:1200", "01JAN1999:1200"]
    with dss.open(out) as f:
        grid = f.read_grid(path)
    assert np.nanmax(grid.data) > 0


def _snodas(tmp_path):
    for ext in (".dat.gz", ".txt.gz"):
        shutil.copyfile(fixture_path(f"p71/{STEM}{ext}"), tmp_path / f"{STEM}{ext}")
    return tmp_path / f"{STEM}.dat.gz"


KW = dict(variable=SNOW, cell_size=2000.0, align="vortex",
          envelope=(-200_000.0, 1_700_000.0, 0.0, 1_900_000.0))


def _record(path, pathname):
    with dss.open(path) as f:
        return f.read_grid(pathname)


def test_snodas_default_times_are_the_header_and_vortex_is_an_option(tmp_path):
    """By default the 24-hour total is the header's 24 hours ending 06:00 UTC, PER-CUM;
    snodas_times="vortex" stamps the 24 hours after, typed PER-AVER, as Vortex does."""
    src = _snodas(tmp_path)
    assert gridded.list_variables(src) == [SNOW]
    default = gridded.import_gridded([src], tmp_path / "d.dss", **KW)
    vortex = gridded.import_gridded([src], tmp_path / "v.dss", snodas_times="vortex", **KW)
    stated = gridded.import_gridded([src], tmp_path / "s.dss", snodas_times="header_as_stated", **KW)
    [d] = default.pathnames
    [v] = vortex.pathnames
    [s] = stated.pathnames
    assert d.split("/")[4:6] == ["31DEC2019:0600", "01JAN2020:0600"]
    assert v.split("/")[4:6] == ["01JAN2020:0600", "02JAN2020:0600"]
    assert s.split("/")[4:6] == ["31DEC2019:0600", "01JAN2020:0600"]
    assert _record(tmp_path / "d.dss", d).data_type_name == "PER-CUM"
    assert _record(tmp_path / "v.dss", v).data_type_name == "PER-AVER"
    assert _record(tmp_path / "s.dss", s).data_type_name == "PER-AVER"
    # Only the times and the type differ: the cells are the same.
    a, b = _record(tmp_path / "d.dss", d).data, _record(tmp_path / "v.dss", v).data
    assert np.array_equal(np.asarray(a), np.asarray(b), equal_nan=True)


def test_snodas_envelope_clip_is_the_normal_path(tmp_path):
    """The whole grid is over the 16,777,216-cell limit and is refused unwarped; the docstring's
    model-envelope clip, with a buffer of four cells, imports, and -9999 cells are missing."""
    src = _snodas(tmp_path)
    with pytest.raises(dss.DssUnsupportedRecordError, match="16777216"):
        gridded.import_gridded([src], tmp_path / "whole.dss", variable=SNOW, target_crs=None,
                               cell_size=None)
    assert not (tmp_path / "whole.dss").exists()
    west, south, east, north = -2_200_000.0, 2_900_000.0, -2_000_000.0, 3_100_000.0
    cell = 2000.0
    pad = 4 * cell
    out = tmp_path / "clip.dss"
    result = gridded.import_gridded(
        [src], out, variable=SNOW, target_crs="SHG", cell_size=cell, method="bilinear",
        envelope=(west - pad, south - pad, east + pad, north + pad),
    )
    [path] = result.pathnames
    grid = _record(out, path)
    assert grid.nx * grid.ny <= 16_777_216
    data = np.asarray(grid.data, dtype=float)
    # The north-west corner of CONUS: the Pacific, outside SNODAS's mask, is missing; no cell holds
    # Vortex's scaled sentinel.
    assert np.isnan(data).any() and np.isfinite(data).any()
    assert not np.isclose(data[np.isfinite(data)], -9999.0 / 100_000.0).any()


@pytest.mark.parametrize("stem", [
    STEM.replace("2020010105DP000", "2020010117DP000"),  # hour 17
    STEM.replace("2020010105DP000", "2020010105DP001"),  # offset
    STEM.replace("2020010105DP000", "2020010105HP000"),  # step
    STEM.replace("2020010105DP000", "2020010205DP000"),  # date
])
def test_snodas_filename_timing_fields_must_agree(tmp_path, stem):
    """The default refuses a file name whose timing field conflicts with
    the real header, names both options, and writes nothing; Vortex's timing still reads it."""
    for ext in (".dat.gz", ".txt.gz"):
        shutil.copyfile(fixture_path(f"p71/{STEM}{ext}"), tmp_path / f"{stem}{ext}")
    src = tmp_path / f"{stem}.dat.gz"
    with pytest.raises(dss.DssUnsupportedRecordError) as e:
        gridded.import_gridded([src], tmp_path / "d.dss", **KW)
    assert 'snodas_times="vortex"' in str(e.value) and '"header_as_stated"' in str(e.value)
    assert not (tmp_path / "d.dss").exists()
    gridded.import_gridded([src], tmp_path / "v.dss", snodas_times="vortex", **KW)


def test_snodas_ambiguous_product_names_the_options(tmp_path):
    """A product with no qualified interpretation (here the 1038 snowpack temperature) is
    refused by default, naming the two options, and nothing is written."""
    stem = "us_ssmv11038wS__A0024TTNATS2020010105DP001"
    shutil.copyfile(fixture_path(f"p71/{STEM}.dat.gz"), tmp_path / f"{stem}.dat.gz")
    shutil.copyfile(fixture_path(f"p71/{STEM}.txt.gz"), tmp_path / f"{stem}.txt.gz")
    src = tmp_path / f"{stem}.dat.gz"
    name = "Snow Pack Average Temperature"
    assert gridded.list_variables(src) == [name]
    kw = dict(KW, variable=name)
    with pytest.raises(dss.DssUnsupportedRecordError) as e:
        gridded.import_gridded([src], tmp_path / "t.dss", **kw)
    assert 'snodas_times="vortex"' in str(e.value) and '"header_as_stated"' in str(e.value)
    assert not (tmp_path / "t.dss").exists()
    [v] = gridded.import_gridded([src], tmp_path / "v.dss", snodas_times="vortex", **kw).pathnames
    assert v.split("/")[4:6] == ["01JAN2020:0600", ""]


def test_snodas_keywords_are_checked(tmp_path):
    src = _snodas(tmp_path)
    for bad in (dict(snodas_times="file"), dict(snodas_georeference="nohrsc")):
        with pytest.raises(ValueError):
            gridded.import_gridded([src], tmp_path / "x.dss", variable=SNOW, **bad)
    (tmp_path / f"{STEM}.txt.gz").unlink()
    for kw in (dict(snodas_georeference="header"), {}, dict(snodas_times="header_as_stated")):
        with pytest.raises(dss.DssUnsupportedRecordError):
            gridded.import_gridded([src], tmp_path / "y.dss", variable=SNOW, **kw)
        assert not (tmp_path / "y.dss").exists()
    # Vortex's timing needs no header.
    gridded.import_gridded([src], tmp_path / "z.dss", snodas_times="vortex", **KW)

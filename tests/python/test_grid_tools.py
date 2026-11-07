"""P53: grid tools through the public API. Public and synthetic inputs only.

Value parity is ``crates/dss-vortex/tests/grid_tools.rs``, on what Vortex 1.1.0 wrote from its own test
resources. This file covers the API: arguments, typed errors, what a record says about itself, locks
and the process. HEC oracle comparisons are local-only (evidence/P53/REPORT.md).
"""

from __future__ import annotations

import datetime as dt
import hashlib
import json
import os
import sys
import warnings

import numpy as np
import pytest

import ras_commander_fast_dss as dss
from ras_commander_fast_dss import DssLockedError, DssUnsupportedRecordError, gridded

from conftest import FIXTURES
from test_lock import holder_factory, ranges  # noqa: F401  (a fixture, used by name)

P53 = FIXTURES / "vortex_p53"
QPE, PRISM, CONSTANT = P53 / "qpe.dss", P53 / "prism.dss", P53 / "constant.tiff"
ONE = "///PRECIPITATION/02JAN2017:1200/02JAN2017:1300//"
H0 = "01JAN2017:0000/01JAN2017:0100"
UNDEF = np.float32(-3.4028235e38)
SHG = dict(grid_type="shg", cell_size=2000.0, lower_left_cell=(-1000, 1000))
SHG_PRJ = (
    'PROJCS["USA_Contiguous_Albers_Equal_Area_Conic_USGS_version",GEOGCS["GCS_North_American_1983",'
    'DATUM["D_North_American_1983",SPHEROID["GRS_1980",6378137.0,298.257222101]],PRIMEM["Greenwich",0.0],'
    'UNIT["Degree",0.0174532925199433]],PROJECTION["Albers"],PARAMETER["False_Easting",0.0],'
    'PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",-96.0],PARAMETER["Standard_Parallel_1",29.5],'
    'PARAMETER["Standard_Parallel_2",45.5],PARAMETER["Latitude_Of_Origin",23.0],UNIT["Meter",1.0]]'
)


def grid_file(path, cells, pathname=None, units="MM", data_type="PER-CUM", version=7):
    from ras_commander_fast_dss._write_grid import write_grid

    if not path.exists():
        dss.create(path, version=version)
    write_grid(path, pathname or f"///PRECIPITATION/{H0}//", np.asarray(cells, dtype=np.float32), units=units,
               data_type=data_type, raw=True, version=version, **SHG)
    return path


def cells_of(path, pathname=None):
    with dss.open(path) as f:
        names = [str(p) for p in f.catalog()["pathname"].tolist()]
        g = f.read_grid(pathname or names[0], missing="raw")
        return np.asarray(g.data, dtype=np.float32).reshape(-1), g


def bits(a):
    return np.asarray(a, dtype=np.float32).view(np.uint32).tolist()


HOLED = [[1.0, UNDEF, 3.0], [4.0, 5.0, 6.5]]


def test_the_feature_is_listed_and_public():
    assert "grid_tools" in dss.FEATURES
    for name in ("calculate", "calculate_raster", "sanitize", "normalize", "clip", "NormalizeResult"):
        assert name in gridded.__all__ and hasattr(gridded, name)


def test_calculate_keeps_missing_cells_and_says_what_the_record_was(tmp_path):
    src = grid_file(tmp_path / "in.dss", HOLED)
    out = tmp_path / "out.dss"
    r = gridded.calculate(src, out, multiply=0.5, parts={"F": "X"})
    assert r.pathnames == [f"///PRECIPITATION/{H0}/X/"]
    row = r.records[0]
    assert (row["source_units"], row["units"], row["converted"]) == ("MM", "MM", False)
    assert (row["source_data_type"], row["data_type"]) == ("PER-CUM", "PER-CUM")
    cells, g = cells_of(out)
    assert bits(cells) == bits([0.5, UNDEF, 1.5, 2.0, 2.5, 3.25])
    assert g.data_units == "MM"


@pytest.mark.parametrize("kw", [{}, {"add": 1.0, "divide": 2.0}])
def test_calculate_takes_exactly_one_operation(tmp_path, kw):
    with pytest.raises(ValueError, match="exactly one"):
        gridded.calculate(tmp_path / "a.dss", tmp_path / "b.dss", **kw)


@pytest.mark.parametrize("kw, text", [
    (dict(divide=0.0), "cannot hold"),
    (dict(multiply=1e38), "cannot hold"),
    (dict(add=float("nan")), "must be finite"),
    (dict(add=1.0, variables=["///PRECIPITATION/09JAN2017:0000/09JAN2017:0100//"]), "does not hold it"),
    (dict(add=1.0, variables=[]), "holds no grids"),
])
def test_a_refusal_is_typed_and_creates_nothing(tmp_path, kw, text):
    src = grid_file(tmp_path / "in.dss", HOLED)
    out = tmp_path / "out.dss"
    with pytest.raises((DssUnsupportedRecordError, ValueError), match=text) as e:
        gridded.calculate(src, out, **kw)
    assert not out.exists()
    if isinstance(e.value, DssUnsupportedRecordError):
        assert e.value.phase == "planning"
        assert e.value.completed in ([], None)


def test_a_refused_value_leaves_an_existing_file_as_it_was(tmp_path):
    src = grid_file(tmp_path / "in.dss", HOLED)
    out = grid_file(tmp_path / "out.dss", HOLED, pathname=f"///OTHER/{H0}//", data_type="PER-AVER")
    before = out.read_bytes()
    with pytest.raises(DssUnsupportedRecordError, match="cannot hold"):
        gridded.calculate(src, out, divide=0.0)
    assert out.read_bytes() == before


def test_a_held_output_is_refused_and_untouched(tmp_path, holder_factory):  # noqa: F811
    src = grid_file(tmp_path / "in.dss", HOLED)
    out = grid_file(tmp_path / "out.dss", HOLED, pathname=f"///OTHER/{H0}//", data_type="PER-AVER")
    before = out.read_bytes()
    _, off, n = ranges(out)[0]
    h = holder_factory(out)
    h.lock(off, n)
    with pytest.raises(DssLockedError) as e:
        gridded.calculate(src, out, add=1.0)
    assert e.value.phase == "planning"
    h.exit()
    assert out.read_bytes() == before


def test_a_change_of_type_must_be_stated_and_a_conversion_is_said(tmp_path):
    src = grid_file(tmp_path / "in.dss", HOLED, pathname=f"///ZZTOTAL/{H0}//")
    with pytest.raises(DssUnsupportedRecordError, match="State data_type"):
        gridded.calculate(src, tmp_path / "a.dss", add=1.0)
    r = gridded.calculate(src, tmp_path / "a.dss", add=1.0, data_type="PER-AVER")
    assert (r.records[0]["source_data_type"], r.records[0]["data_type"]) == ("PER-CUM", "PER-AVER")

    kelvin = grid_file(tmp_path / "k.dss", [[300.0] * 3] * 2, pathname=f"///TEMPERATURE/{H0}//", units="K",
                       data_type="PER-AVER")
    with pytest.warns(RuntimeWarning, match="converted"):
        r = gridded.calculate(kelvin, tmp_path / "c.dss", add=1.0)
    assert (r.records[0]["source_units"], r.records[0]["units"], r.records[0]["converted"]) == ("K", "DEG C", True)
    with warnings.catch_warnings():
        warnings.simplefilter("error")
        gridded.calculate(src, tmp_path / "quiet.dss", add=1.0, data_type="PER-AVER")


def test_parts_are_vortexs_and_f_star_keeps_the_source(tmp_path):
    src = grid_file(tmp_path / "in.dss", HOLED, pathname=f"/KEEP/KEEP/PRECIPITATION/{H0}/RUN7/")
    r = gridded.calculate(src, tmp_path / "a.dss", add=1.0)
    assert r.pathnames == [f"///PRECIPITATION/{H0}//"]
    r = gridded.calculate(src, tmp_path / "b.dss", add=1.0, parts={"F": "*"})
    assert r.pathnames == [f"///PRECIPITATION/{H0}/RUN7/"]
    with pytest.raises(ValueError, match="parts may hold"):
        gridded.calculate(src, tmp_path / "c.dss", add=1.0, parts={"D": "x"})


def test_raster_calculator_on_vortexs_resource(tmp_path):
    out = tmp_path / "out.dss"
    r = gridded.calculate_raster(QPE, CONSTANT, out, operation="multiply", method="bilinear", variables=[ONE], parts={"F": "M"})
    assert len(r.records) == 1
    done, _ = cells_of(out)
    source, _ = cells_of(QPE, ONE)
    assert bits(done) == bits(source * np.float32(2.0))
    # BatchGridCalculatorTest: the source's maximum is 1.7.
    assert abs(float(done.max()) - 3.4) < 1e-3
    with pytest.raises(ValueError, match="operation must be"):
        gridded.calculate_raster(QPE, CONSTANT, tmp_path / "x.dss", operation="power")
    with pytest.raises(ValueError, match="method must be"):
        gridded.calculate_raster(QPE, CONSTANT, tmp_path / "x.dss", operation="add", method="cubic")


def test_sanitize_is_inclusive_and_leaves_missing_cells(tmp_path):
    src = grid_file(tmp_path / "in.dss", [[0.0, UNDEF, 1.0], [2.0, 3.0, 4.0]])
    out = tmp_path / "out.dss"
    gridded.sanitize(src, out, minimum=1.0, minimum_replacement=7.0, maximum=3.0, maximum_replacement=8.0)
    assert bits(cells_of(out)[0]) == bits([7.0, UNDEF, 7.0, 2.0, 8.0, 8.0])
    out = tmp_path / "default.dss"
    gridded.sanitize(src, out, minimum=1.0)
    assert bits(cells_of(out)[0]) == bits([UNDEF, UNDEF, UNDEF, 2.0, 3.0, 4.0])


def test_normalize_on_vortexs_resource(tmp_path):
    out = tmp_path / "out.dss"
    r = gridded.normalize(QPE, PRISM, out, start=dt.datetime(2017, 1, 1), end=dt.datetime(2017, 1, 3),
                          interval=dt.timedelta(days=1), parts={"F": "N"})
    # NormalizerPassesRegression: 48 paths. The 49th ends at the first day's start, and lies in no interval.
    assert len(r.records) == 48
    assert r.sources_left_out == ["///PRECIPITATION/31DEC2016:2300/31DEC2016:2400//"]
    assert r.normals_left_out == []
    assert [(i["sources"], i["normals"]) for i in r.intervals] == [(24, 1), (24, 1)]
    assert not r.long_hash_bin
    assert {row["data_type"] for row in r.records} == {"PER-CUM"}
    # prism.dss says `mm`, and the units of a record are its source's, spelled as HEC does.
    assert {row["units"] for row in r.records} == {"MM"}
    # The day's cells now total the normal, cell by cell, within float rounding.
    with dss.open(out) as f:
        names = [str(p) for p in f.catalog()["pathname"].tolist() if "02JAN2017" in str(p)]
        total = sum(np.asarray(f.read_grid(n, missing="raw").data, dtype=np.float64) for n in names)
    normal, _ = cells_of(PRISM, "///PRECIPITATION/02JAN2017:0000/02JAN2017:2400//")
    wet = total.reshape(-1) > 0
    assert wet.sum() > 100
    assert np.allclose(total.reshape(-1)[wet], normal[wet], rtol=1e-5)


@pytest.mark.parametrize("interval", [0, -3600, 0.5])
def test_normalize_needs_a_real_interval(tmp_path, interval):
    with pytest.raises(ValueError):
        gridded.normalize(QPE, PRISM, tmp_path / "x.dss", start=dt.datetime(2017, 1, 1), end=dt.datetime(2017, 1, 3),
                          interval=interval)


def test_normalize_times_must_be_naive(tmp_path):
    with pytest.raises(ValueError, match="naive"):
        gridded.normalize(QPE, PRISM, tmp_path / "x.dss", start=dt.datetime(2017, 1, 1, tzinfo=dt.timezone.utc),
                          end=dt.datetime(2017, 1, 3), interval=86400)


def test_clip_needs_a_stated_crs_and_whole_cells(tmp_path):
    src = grid_file(tmp_path / "in.dss", HOLED)
    x, y = -2000000.0, 2000000.0
    wkt = "SHG"
    with pytest.raises(ValueError, match="envelope_crs"):
        gridded.clip(src, tmp_path / "a.dss", envelope=(x, y, x + 2000, y + 2000))
    with pytest.raises(ValueError, match="or a shapefile"):
        gridded.clip(src, tmp_path / "a.dss")
    out = tmp_path / "ok.dss"
    gridded.clip(src, out, envelope=(x + 2000, y, x + 6000, y + 2000), envelope_crs=wkt)
    cells, g = cells_of(out)
    assert bits(cells) == bits([UNDEF, 3.0])
    assert (g.nx, g.ny, g.lower_left_cell_x, g.lower_left_cell_y) == (2, 1, -999, 1000)
    off = (x + 2100, y + 100, x + 6100, y + 2100)
    with pytest.raises(DssUnsupportedRecordError, match="whole cells"):
        gridded.clip(src, tmp_path / "b.dss", envelope=off, envelope_crs=wkt)
    assert not (tmp_path / "b.dss").exists()
    gridded.clip(src, tmp_path / "c.dss", envelope=off, envelope_crs=wkt, align="vortex")
    assert (tmp_path / "c.dss").exists()


def test_dss6_in_and_out(tmp_path):
    src = grid_file(tmp_path / "in.dss", HOLED, version=6)
    out = tmp_path / "out.dss"
    gridded.calculate(src, out, multiply=2.0, version=6)
    with dss.open(out) as f:
        assert int(f.version) == 6
    assert bits(cells_of(out)[0]) == bits([2.0, UNDEF, 6.0, 8.0, 10.0, 13.0])


@pytest.mark.parametrize("subtract, reserved", [(902.0, -901.0), (903.0, -902.0)])
def test_a_result_the_reader_takes_as_missing_is_refused(tmp_path, subtract, reserved):
    source = grid_file(tmp_path / "in.dss", [[1.0, 2.0, 3.0], [4.0, 5.0, 6.0]])
    existing = grid_file(tmp_path / "existing.dss", [[9.0] * 3] * 2)
    before = existing.read_bytes()
    with pytest.raises(DssUnsupportedRecordError, match="take as missing"):
        gridded.calculate(source, existing, subtract=subtract)
    assert existing.read_bytes() == before
    with pytest.raises(ValueError, match="take as missing"):
        gridded.sanitize(source, tmp_path / "san.dss", minimum=1.0, minimum_replacement=reserved)
    assert not (tmp_path / "san.dss").exists()


def test_a_chain_round_trips_every_value_it_writes(tmp_path):
    """Nothing a run reports as valid may read back as missing, by default or in a next operation."""
    source = grid_file(tmp_path / "in.dss", [[1.0, 2.0, 3.0], [4.0, 5.0, 6.0]])
    first = tmp_path / "first.dss"
    gridded.calculate(source, first, subtract=900.0)
    with dss.open(first) as f:
        read = np.asarray(f.read_grid(str(f.catalog()["pathname"].iloc[0])).data).reshape(-1)
    assert not np.isnan(read).any()
    assert read.tolist() == [-899.0, -898.0, -897.0, -896.0, -895.0, -894.0]
    second = tmp_path / "second.dss"
    gridded.calculate(first, second, multiply=2.0)
    assert cells_of(second)[0].tolist() == [-1798.0, -1796.0, -1794.0, -1792.0, -1790.0, -1788.0]
    with pytest.raises(DssUnsupportedRecordError, match="take as missing"):
        gridded.calculate(first, tmp_path / "third.dss", subtract=2.0)
    assert not (tmp_path / "third.dss").exists()


def test_the_importer_refuses_a_value_the_reader_would_lose(tmp_path):
    grid = tmp_path / "in.asc"
    grid.write_text("ncols 2\nnrows 1\nxllcorner 0\nyllcorner 0\ncellsize 2000\nNODATA_value -9999\n-901 1\n")
    grid.with_suffix(".prj").write_text(SHG_PRJ)
    out = tmp_path / "out.dss"
    with pytest.raises(DssUnsupportedRecordError, match="take as missing"):
        gridded.import_gridded([grid], out, variable=gridded.list_variables(grid)[0], parts={"C": "PRECIPITATION"})
    assert not out.exists()


def test_a_hard_link_to_an_input_is_refused(tmp_path):
    source = grid_file(tmp_path / "in.dss", [[1.0, 2.0, 3.0], [4.0, 5.0, 6.0]])
    alias = tmp_path / "alias.dss"
    os.link(source, alias)
    before = source.read_bytes()
    with pytest.raises(DssUnsupportedRecordError, match="also the input"):
        gridded.calculate(source, alias, multiply=2.0)
    assert source.read_bytes() == before and alias.read_bytes() == before


def test_a_dss_raster_at_another_time_needs_static_raster(tmp_path):
    source = grid_file(tmp_path / "in.dss", [[1.0, 2.0, 3.0], [4.0, 5.0, 6.0]])
    raster = grid_file(tmp_path / "raster.dss", [[2.0] * 3] * 2, pathname="///PRECIPITATION/02JAN2017:0000/02JAN2017:0100//")
    out = tmp_path / "out.dss"
    with pytest.raises(DssUnsupportedRecordError, match="static_raster"):
        gridded.calculate_raster(source, raster, out, operation="multiply", method="near")
    assert not out.exists()
    done = gridded.calculate_raster(source, raster, out, operation="multiply", method="near", static_raster=True)
    assert len(done.records) == 1
    assert cells_of(out)[0].tolist() == [2.0, 4.0, 6.0, 8.0, 10.0, 12.0]


def normalized(tmp_path, source, normal, name="out.dss", version=None):
    a = grid_file(tmp_path / f"{name}.in.dss", source)
    n = grid_file(tmp_path / f"{name}.n.dss", normal)
    extra = {} if version is None else {"version": version}
    return gridded.normalize(a, n, tmp_path / name, start=dt.datetime(2017, 1, 1), end=dt.datetime(2017, 1, 1, 1), interval=3600, **extra)


def test_a_missing_normal_is_written_missing_counted_and_said(tmp_path):
    with pytest.warns(RuntimeWarning, match="2 valid cell.*written as missing"):
        done = normalized(tmp_path, [[1.0, 2.0, 3.0], [4.0, 5.0, 6.0]], [[UNDEF, 2.0, 2.0], [2.0, UNDEF, 2.0]])
    cells, grid = cells_of(tmp_path / "out.dss")
    assert bits(cells) == bits([UNDEF, 2.0, 2.0, 2.0, UNDEF, 2.0])
    assert done.cells_made_missing_by_normal == 2
    assert grid.min_value == 2.0 and grid.max_value == 2.0 and grid.mean_value == 2.0
    with warnings.catch_warnings():
        warnings.simplefilter("error")
        done = normalized(tmp_path, [[1.0, 2.0, 3.0], [4.0, 5.0, 6.0]], [[2.0] * 3] * 2, name="out2.dss")
    assert done.cells_made_missing_by_normal == 0


@pytest.mark.parametrize("version", [7, 6])
def test_a_normal_missing_everywhere_writes_no_valid_cell(tmp_path, version):
    with pytest.warns(RuntimeWarning, match="5 valid cell"):
        done = normalized(tmp_path, [[1.0, 2.0, UNDEF], [0.0, 5.0, 6.0]], [[UNDEF] * 3] * 2, version=version)
    assert len(done.records) == 1 and done.cells_made_missing_by_normal == 5
    cells, _ = cells_of(tmp_path / "out.dss")
    assert bits(cells) == bits([UNDEF] * 6)
    with dss.open(tmp_path / "out.dss") as f:
        assert np.isnan(f.read_grid(done.pathnames[0]).data).all()


def test_a_source_cell_missing_too_stays_missing_and_is_not_counted(tmp_path):
    with pytest.warns(RuntimeWarning, match="1 valid cell"):
        done = normalized(tmp_path, [[UNDEF, 2.0, 3.0], [UNDEF, 5.0, 6.0]], [[UNDEF, UNDEF, 2.0], [2.0, 2.0, 2.0]])
    assert done.cells_made_missing_by_normal == 1
    assert bits(cells_of(tmp_path / "out.dss")[0]) == bits([UNDEF, UNDEF, 2.0, UNDEF, 2.0, 2.0])


def test_a_zero_total_with_no_normal_is_missing_and_not_zero(tmp_path):
    with pytest.warns(RuntimeWarning, match="1 valid cell"):
        done = normalized(tmp_path, [[0.0, 0.0, 3.0], [4.0, 5.0, 6.0]], [[UNDEF, 2.0, 3.0], [4.0, 5.0, 6.0]])
    assert done.cells_made_missing_by_normal == 1
    assert bits(cells_of(tmp_path / "out.dss")[0]) == bits([UNDEF, 0.0, 3.0, 4.0, 5.0, 6.0])


def test_the_old_name_of_the_count_is_gone():
    assert "passed_through_cells" not in gridded.NormalizeResult.__dataclass_fields__
    assert "written as missing" in " ".join(gridded.normalize.__doc__.split())


def test_hrap_and_other_albers_grids_are_computed(tmp_path):
    from ras_commander_fast_dss._write_grid import write_grid

    cells = np.array([[1.0, 2.0, 3.0], [4.0, 5.0, 6.0]], dtype=np.float32)
    hrap = tmp_path / "hrap.dss"
    dss.create(hrap, version=7)
    write_grid(hrap, f"///PRECIPITATION/{H0}//", cells, grid_type="hrap", cell_size=4762.5, lower_left_cell=(300, 400), units="MM", data_type="PER-CUM", raw=True)
    out = tmp_path / "out.dss"
    gridded.calculate(hrap, out, multiply=2.0)
    read, grid = cells_of(out)
    assert read.tolist() == [2.0, 4.0, 6.0, 8.0, 10.0, 12.0]
    assert 'PARAMETER["false_easting",1909762.5]' in grid.srs_definition
    assert (grid.lower_left_cell_x, grid.lower_left_cell_y) == (300, 400)


def test_no_java_enters_the_process(tmp_path):
    src = grid_file(tmp_path / "in.dss", HOLED)
    gridded.calculate(src, tmp_path / "out.dss", add=1.0)
    assert not {"jpype", "jnius", "py4j"} & set(sys.modules)


def test_fixture_is_what_the_readme_lists():
    expected = json.loads((P53 / "expected.json").read_text(encoding="utf-8"))
    assert expected["source"].startswith("Vortex 1.1.0")
    assert expected["case_count"] == len(expected["cases"]) >= 10
    assert expected["record_count"] == sum(len(c["records"]) for c in expected["cases"])
    readme = (FIXTURES / "README.md").read_text(encoding="utf-8")
    listed = {}
    for line in readme.splitlines():
        cells = [c.strip(" `") for c in line.split("|")]
        if len(cells) > 5 and cells[1].startswith("vortex_p53/"):
            listed[cells[1].split("/", 1)[1]] = cells[5]
    on_disk = {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in P53.iterdir()}
    assert on_disk and listed == on_disk


def test_the_calculator_words_albers_angles_as_vortex_stores_them(tmp_path):
    source = tmp_path / "in.dss"
    dss.create(source)
    dss.write_grid(source, "///PRECIPITATION/01JAN2024:0000/01JAN2024:0100//", np.full((2, 3), 1.5, dtype=np.float32), units="MM",
                   data_type="PER-CUM", grid_type="albers", cell_size=2000.0, lower_left_cell=(1, 2), compression="zlib",
                   albers=dict(first_standard_parallel=29.3, second_standard_parallel=45.7, central_meridian=-100.123,
                               latitude_of_origin=23.1, false_easting=500000.5, false_northing=-1234.75, datum=2, units="METERS"))
    out = tmp_path / "out.dss"
    gridded.calculate(source, out, multiply=1.0)
    with dss.open(out) as f:
        text = f.read_grid("///PRECIPITATION/01JAN2024:0000/01JAN2024:0100//").srs_definition
    # Vortex 1.1.0's and 0.13.3's record: `calc.albers_odd`.
    assert 'PARAMETER["standard_parallel_1",29.2999992370606]' in text
    assert 'PARAMETER["standard_parallel_2",45.7000007629395]' in text
    assert 'PARAMETER["false_easting",500000.5]' in text

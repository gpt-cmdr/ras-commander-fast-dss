"""Grid to point on the wheel: API, errors, and read-back through this library's own reader.

``fixtures/public/vortex_p51/`` holds Vortex's public ``canyon`` test set, synthetic zones and grids,
and in ``expected.json`` what Vortex 1.1.0 computed. Value parity is
``crates/dss-vortex/tests/zonal.rs``. These tests do not run Vortex or HEC's readers.
"""
import hashlib
import json
import shutil
from pathlib import Path

import numpy as np
import pytest

import ras_commander_fast_dss as dss
from ras_commander_fast_dss import exceptions, gridded

P51 = Path(__file__).resolve().parents[2] / "fixtures" / "public" / "vortex_p51"
MISSING = dss.HEC_UNDEFINED_FLOAT


def grids():
    return P51 / "regular_1h.dss"


def polys():
    return P51 / "polys.shp"


def test_the_fixture_is_what_the_readme_lists():
    readme = (P51.parents[0] / "README.md").read_text(encoding="utf-8")
    files = sorted(p for p in P51.iterdir())
    assert files
    for path in files:
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        assert f"`vortex_p51/{path.name}`" in readme, path.name
        assert digest in readme, path.name


def test_canyon_gives_vortexs_pathnames_and_stamps(tmp_path):
    out = tmp_path / "canyon_ts.dss"
    result = gridded.grid_to_point(
        P51 / "Canyon_PRISM.dss", P51 / "canyon.shp", "Name", out, statistics=gridded.STATISTICS, accumulate=True
    )
    assert result.frames == 3
    assert sorted(r["pathname"] for r in result.records) == sorted(
        f"//canyon/{c}//1Day//"
        for c in (
            "PRECIP-INC",
            "PRECIP-INC-MIN",
            "PRECIP-INC-MAX",
            "PRECIP-INC-MEDIAN",
            "PRECIP-INC-Q1",
            "PRECIP-INC-Q3",
            "PRECIP-INC-PCT>0",
            "PRECIP-INC-PCT>Q1",
            "PRECIP-CUM",
        )
    )
    assert result.zones[0]["zone"] == "canyon" and len(result.zones[0]["cells"]) == 922
    with dss.DssFile(out) as f:
        d = f.read_arrays("//canyon/PRECIP-INC//1Day//")
        assert len(d["values"]) >= 3
        frame = f.read_timeseries("//canyon/PRECIP-INC//1Day//")
    # GridToPointConverterTest: the first stamp is 21 June 1997, 12:00, one day after its start.
    assert str(frame.index[0]).startswith("1997-06-21 12:00")
    assert len(frame) == 3


def test_a_regular_series_has_period_end_stamps_and_a_single_frame_is_irregular(tmp_path):
    out = tmp_path / "out.dss"
    gridded.grid_to_point(grids(), polys(), "Name", out, parts={"A": "A1", "F": "F1"})
    with dss.DssFile(out) as f:
        frame = f.read_timeseries("/A1/SQUARE/PRECIP-INC//1Hour/F1/")
    assert str(frame.index[0]).startswith("2020-03-01 01:00")
    assert len(frame) == 6


def test_statistics_table_and_missing_marker(tmp_path):
    table = gridded.zonal_statistics(grids(), polys(), "Name")
    assert list(table.columns[:5]) == ["zone", "variable", "units", "start", "end"]
    outside = table[table.zone == "OUTSIDE"]
    assert outside["average"].isna().all(), "a zone with no cell is missing, not 0"
    raw = gridded.zonal_statistics(grids(), polys(), "Name", raw=True)
    assert (raw[raw.zone == "OUTSIDE"]["average"] == float(MISSING)).all()
    square = table[table.zone == "SQUARE"]
    assert (square["pct_gt_0"] > 0).all() and (square["average"] > 0).all()


def test_zone_masks_are_flat_indices():
    masks = gridded.zone_masks(grids(), polys(), "Name")
    assert len(masks["SQUARE"]) == 6 * 4
    assert masks["SQUARE"][0] == 2 * 60 + 2
    assert masks["OUTSIDE"].size == 0


def test_area_weighted_is_a_separate_named_option(tmp_path):
    out = tmp_path / "a.dss"
    with pytest.raises(ValueError, match="area_weighted=True"):
        gridded.grid_to_point(grids(), polys(), "Name", out, statistics=["area_weighted_mean"])
    assert not out.exists()
    # This zone file has two polygons over one cell on purpose, which would count the cell twice.
    with pytest.raises(exceptions.DssUnsupportedRecordError, match="overlap"):
        gridded.grid_to_point(grids(), polys(), "Name", out, statistics=["average"], area_weighted=True)
    assert not out.exists()
    canyon = (P51 / "Canyon_PRISM.dss", P51 / "canyon.shp", "Name")
    result = gridded.grid_to_point(*canyon, out, statistics=["average"], area_weighted=True)
    names = {r["pathname"] for r in result.records}
    assert names == {"//canyon/PRECIP-INC//1Day//", "//canyon/PRECIP-INC-AREAWT//1Day//"}
    table = gridded.zonal_statistics(*canyon, area_weighted=True)
    assert "area_weighted_mean" in table.columns
    # It answers another question: close to the cell-centre mean here, and not the same number.
    assert (table["area_weighted_mean"] != table["average"]).all()
    assert (abs(table["area_weighted_mean"] / table["average"] - 1) < 0.05).all()
    assert "area_weighted_mean" not in gridded.zonal_statistics(grids(), polys(), "Name").columns


def test_arguments_are_checked_before_any_output(tmp_path):
    out = tmp_path / "x.dss"
    for bad in (
        dict(statistics=["mode"]),
        dict(statistics=[]),
        dict(statistics=["min", "min"]),
        dict(parts={"B": "X"}),
        dict(parts={"A": "a/b"}),
        dict(compatibility="other"),
        dict(version=5),
    ):
        with pytest.raises(ValueError):
            gridded.grid_to_point(grids(), polys(), "Name", out, **bad)
        assert not out.exists(), bad


def test_a_missing_field_is_refused_and_names_what_exists(tmp_path):
    out = tmp_path / "x.dss"
    with pytest.raises(exceptions.DssUnsupportedRecordError, match="CODE") as e:
        gridded.grid_to_point(grids(), polys(), "NOPE", out)
    assert e.value.phase == "planning"
    assert not out.exists()


def test_missing_inputs_are_os_errors(tmp_path):
    with pytest.raises(OSError):
        gridded.grid_to_point(tmp_path / "none.dss", polys(), "Name", tmp_path / "x.dss")
    with pytest.raises(OSError):
        gridded.grid_to_point(grids(), tmp_path / "none.shp", "Name", tmp_path / "x.dss")


def test_other_records_stay_and_a_rerun_is_unchanged(tmp_path):
    out = tmp_path / "copy.dss"
    shutil.copyfile(grids(), out)
    with dss.DssFile(out) as f:
        before = sorted(f.pathnames())
        grid = np.array(f.read_grid(before[0]).data, copy=True)
    first = gridded.grid_to_point(grids(), polys(), "Name", out)
    assert {r["outcome"] for r in first.records} == {"added"}
    with dss.DssFile(out) as f:
        after = f.pathnames()
        assert set(before) < set(after)
        assert np.array_equal(np.asarray(f.read_grid(before[0]).data), grid, equal_nan=True)
    again = gridded.grid_to_point(grids(), polys(), "Name", out)
    assert {r["outcome"] for r in again.records} == {"unchanged"}
    with dss.DssFile(out) as f:
        assert sorted(f.pathnames()) == sorted(after)


def test_other_missing_markers_differ_from_vortex_only_when_asked(tmp_path):
    """-901, -902 and NaN: missing here, numbers in Vortex's statistics, which `compatibility` restores."""
    grid = tmp_path / "odd.dss"
    dss.create(grid)
    cells = np.full((40, 60), 5.0, dtype="float32")
    cells[2, 2], cells[2, 3], cells[2, 4] = -901.0, -902.0, float("nan")
    dss.write_grid(
        grid,
        "/SHG/SYN/PRECIPITATION///ODD/",
        cells,
        grid_type="shg",
        cell_size=2000.0,
        units="MM",
        data_type="PER-CUM",
        lower_left_cell=(-50, 460),
        start="2020-03-01 00:00",
        end="2020-03-01 01:00",
        north_up=True,
        raw=True,
    )
    ours = gridded.zonal_statistics(grid, polys(), "Name")
    theirs = gridded.zonal_statistics(grid, polys(), "Name", compatibility="vortex")
    a = ours[ours.zone == "SQUARE"].iloc[0]
    b = theirs[theirs.zone == "SQUARE"].iloc[0]
    assert a["min"] == 5.0 and a["max"] == 5.0 and a["average"] == 5.0
    assert b["min"] == -902.0 and np.isnan(b["average"])
    # Every other zone is untouched by the choice.
    c = ours[ours.zone == "CCW_OUTER"].iloc[0]
    d = theirs[theirs.zone == "CCW_OUTER"].iloc[0]
    assert c["average"] == d["average"] == 5.0


def test_unknown_variable_stores_vortexs_data_type(tmp_path):
    out = tmp_path / "out.dss"
    source = tmp_path / "unknown.dss"
    gridded.import_arrays(
        np.full((3, 40, 60), 2.0, dtype="float32"),
        source,
        times=[(f"2020-03-01 0{i}:00", f"2020-03-01 0{i + 1}:00") for i in range(3)],
        transform=(-100000.0, 2000.0, 1000000.0, -2000.0),
        crs="SHG",
        variable="MY-THING",
        source_units="MM",
        parts={"A": "SHG", "B": "SYN", "C": "MY-THING", "F": "SYN"},
        units="MM",
    )
    r = gridded.grid_to_point(source, polys(), "Name", out)
    assert {x["data_type"] for x in r.records} == {"INVALID"}
    assert r.records[0]["pathname"].startswith("//") and "/MY-THING//1Hour//" in r.records[0]["pathname"]


def test_dss6_output_reads_back(tmp_path):
    out = tmp_path / "six.dss"
    gridded.grid_to_point(grids(), polys(), "Name", out, version=6)
    with dss.DssFile(out) as f:
        assert f.version == 6
        frame = f.read_timeseries("//SQUARE/PRECIP-INC//1HOUR//")
    assert len(frame) == 6


def test_expected_json_has_something_in_it():
    expected = json.loads((P51 / "expected.json").read_text())
    assert expected["vortex"] == "1.1.0"
    assert {c["id"] for c in expected["cases"]} >= {"canyon", "zones.polys", "zones.random"}


# ---- Regressions: reserved values, aliases, reruns, failure reporting. ----

TRANSFORM = (-100000.0, 2000.0, 1000000.0, -2000.0)
ULP_NEAR_901 = 2.0**-14


def write_arrays(path, data, times, c_part="TEMPERATURE", units="DEG C"):
    gridded.import_arrays(
        np.asarray(data, dtype="f4"),
        path,
        times=times,
        transform=TRANSFORM,
        crs="SHG",
        variable=c_part,
        source_units=units,
        parts={"A": "SHG", "B": "SYN", "C": c_part, "F": "SYN"},
        units=units,
        allow_undetectable_readers=True,
    )


def hours(start, count):
    t = np.datetime64(start)
    step = np.timedelta64(1, "h")
    return [
        ((t + i * step).astype("datetime64[s]").item(), (t + (i + 1) * step).astype("datetime64[s]").item())
        for i in range(count)
    ]


def square_grid(tmp_path, cells, name="reserved.dss"):
    """Two frames at -900, with `cells` in zone SQUARE of the second."""
    probe = tmp_path / "probe.dss"
    write_arrays(probe, np.full((2, 40, 60), -900.0), hours("2020-03-01T00:00", 2))
    mask = gridded.zone_masks(probe, polys(), "Name")["SQUARE"]
    data = np.full((2, 40, 60), -900.0, dtype="f4")
    # A mask's flat index is in the row order the source states, here as `data` runs.
    data[1].flat[np.asarray(mask, dtype=np.int64)] = cells(len(mask))
    path = tmp_path / name
    write_arrays(path, data, hours("2020-03-01T00:00", 2))
    return path


def mean_at(target):
    return lambda n: [-900.0] * (n - 1) + [-900.0 + (target + 900.0) * n]


def mean_rounding_to(target):
    """A mean that is not `target` in doubles, and is in float32: the cast creates the marker."""

    def cells(n):
        # Steps summing to -1, none of them 0: no cell is itself the marker.
        steps = [1, -2] + [1, -1] * ((n - 2) // 2) if n % 2 == 0 else [1, 1, -3] + [1, -1] * ((n - 3) // 2)
        return [target + s * ULP_NEAR_901 for s in steps]

    return cells


@pytest.mark.parametrize("version", [7, 6])
@pytest.mark.parametrize("target", [-901.0, -902.0])
@pytest.mark.parametrize("make", [mean_at, mean_rounding_to])
def test_a_statistic_a_reader_takes_as_missing_is_refused_before_the_output(tmp_path, version, target, make):
    grid = square_grid(tmp_path, make(target))
    table = gridded.zonal_statistics(grid, polys(), "Name", raw=True)
    average = float(table[table.zone == "SQUARE"]["average"].iloc[1])
    assert np.float32(average) == target, "the input must produce the collision"
    # A new output is not created.
    new = tmp_path / "new.dss"
    with pytest.raises(exceptions.DssError) as caught:
        gridded.grid_to_point(grid, polys(), "Name", new, version=version)
    text = str(caught.value)
    assert "SQUARE" in text and "average" in text and "2020-03-01T02:00" in text, text
    assert caught.value.phase == "planning" and caught.value.completed == [] and caught.value.in_doubt is None
    assert not new.exists()
    # An existing output keeps every byte.
    existing = tmp_path / "existing.dss"
    gridded.grid_to_point(grids(), polys(), "Name", existing, version=version)
    before = existing.read_bytes()
    with pytest.raises(exceptions.DssError):
        gridded.grid_to_point(grid, polys(), "Name", existing)
    assert existing.read_bytes() == before


@pytest.mark.parametrize("version", [7, 6])
def test_a_statistic_beside_a_marker_writes_and_reads_back(tmp_path, version):
    grid = square_grid(tmp_path, mean_at(-901.5))
    out = tmp_path / "out.dss"
    result = gridded.grid_to_point(grid, polys(), "Name", out, version=version)
    assert result.reserved_values == 0
    square = next(r["pathname"] for r in result.records if r["zone"] == "SQUARE")
    with dss.DssFile(out) as f:
        frame = f.read_timeseries(square)
    assert list(frame.iloc[:, 0]) == [-900.0, -901.5]


@pytest.mark.parametrize("version", [7, 6])
def test_any_statistic_can_collide_and_is_named(tmp_path, version):
    grid = square_grid(tmp_path, lambda n: [-902.0 + (i - n // 2 + 0.5) for i in range(n)])
    out = tmp_path / "out.dss"
    table = gridded.zonal_statistics(grid, polys(), "Name", raw=True)
    row = table[table.zone == "SQUARE"].iloc[1]
    assert row["median"] == -902.0
    with pytest.raises(exceptions.DssError, match="zone SQUARE, statistic median"):
        gridded.grid_to_point(grid, polys(), "Name", out, statistics=["median"], version=version)
    assert not out.exists()
    # Without the colliding statistic the same grid is written.
    gridded.grid_to_point(grid, polys(), "Name", out, statistics=["max"], version=version)
    assert out.exists()


@pytest.mark.parametrize("version", [7, 6])
def test_an_accumulated_total_a_reader_takes_as_missing_is_refused(tmp_path, version):
    grid = tmp_path / "rain.dss"
    data = np.full((2, 40, 60), 0.0, dtype="f4")
    data[0] = -900.0
    data[1] = -1.0
    write_arrays(grid, data, hours("2020-03-01T00:00", 2), "PRECIPITATION", "MM")
    out = tmp_path / "out.dss"
    with pytest.raises(exceptions.DssError, match="statistic accumulated, time 2020-03-01T02:00"):
        gridded.grid_to_point(grid, polys(), "Name", out, accumulate=True, version=version)
    assert not out.exists()
    gridded.grid_to_point(grid, polys(), "Name", out, accumulate=False, version=version)
    assert out.exists()


def test_vortexs_route_stores_a_reserved_value_and_counts_it(tmp_path):
    grid = square_grid(tmp_path, mean_at(-901.0))
    out = tmp_path / "out.dss"
    result = gridded.grid_to_point(grid, polys(), "Name", out, compatibility="vortex")
    assert result.reserved_values == 1
    square = next(r["pathname"] for r in result.records if r["zone"] == "SQUARE")
    with dss.DssFile(out) as f:
        frame = f.read_timeseries(square)
    assert list(frame.iloc[:, 0]) == [-900.0], "the reader trims what Vortex's route stored"


def test_an_output_that_is_an_input_needs_stating(tmp_path):
    source = tmp_path / "source.dss"
    shutil.copyfile(grids(), source)
    link = tmp_path / "link.dss"
    try:
        link.hardlink_to(source)
    except OSError:
        pytest.skip("no hard links here")
    before = source.read_bytes()
    for output in (source, link):
        with pytest.raises(exceptions.DssError, match="also the input") as caught:
            gridded.grid_to_point(source, polys(), "Name", output)
        assert caught.value.phase == "planning"
        assert source.read_bytes() == before
    done = gridded.grid_to_point(source, polys(), "Name", link, allow_in_place=True)
    assert done.records and source.read_bytes() != before


@pytest.mark.parametrize("version", [7, 6])
def test_a_shorter_rerun_is_a_window_update_and_says_so(tmp_path, version):
    full, short, out = tmp_path / "full.dss", tmp_path / "short.dss", tmp_path / "out.dss"
    write_arrays(full, np.full((6, 40, 60), 10.0), hours("2020-03-01T00:00", 6), "PRECIPITATION", "MM")
    write_arrays(short, np.full((2, 40, 60), 20.0), hours("2020-03-01T02:00", 2), "PRECIPITATION", "MM")
    first = gridded.grid_to_point(full, polys(), "Name", out, version=version)
    assert {r["outcome"] for r in first.records} == {"added"}
    second = gridded.grid_to_point(short, polys(), "Name", out)
    # A zone with no cell stored its missing marker both times, which is no change.
    empty = {z["zone"] for z in second.zones if len(z["cells"]) == 0}
    assert empty and "SQUARE" not in empty
    for r in second.records:
        assert r["values"] == 2
        assert r["outcome"] == ("unchanged" if r["zone"] in empty else "window_updated"), r
    square = next(r["pathname"] for r in second.records if r["zone"] == "SQUARE")
    with dss.DssFile(out) as f:
        frame = f.read_timeseries(square)
    assert list(frame.iloc[:, 0]) == [10.0, 10.0, 20.0, 20.0, 10.0, 10.0]


def test_a_refusal_reports_nothing_written_on_the_exception(tmp_path):
    with pytest.raises(exceptions.DssError) as caught:
        gridded.grid_to_point(grids(), polys(), "NOPE", tmp_path / "x.dss")
    assert caught.value.phase == "planning"
    assert caught.value.completed == [] and caught.value.in_doubt is None

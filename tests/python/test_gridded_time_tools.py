"""The time tools' Python API: arguments, stamps per type, volume, refusals. No Java, no HEC software."""

import datetime as dt
import threading

import numpy as np
import pytest
import ras_commander_fast_dss as dss
from ras_commander_fast_dss import exceptions, gridded

T = dt.datetime(2024, 1, 1)
HOUR = dt.timedelta(hours=1)
MARKER = np.float32(-3.4028235e38)
TRANSFORM = (2000.0, 2000.0, 8000.0, -2000.0)
SHAPE = (2, 3)


def series(path, frames=4, variable="PRECIPITATION", units="mm", first=T, step=HOUR, instants=False, **kw):
    data = np.zeros((frames, *SHAPE), dtype=np.float32)
    for i in range(frames):
        data[i] = (np.arange(6, dtype=np.float32).reshape(SHAPE) + 10 * i) * 0.25
    times = [first + i * step if instants else (first + i * step, first + (i + 1) * step) for i in range(frames)]
    kw.setdefault("parts", {})
    gridded.import_arrays(data, path, times=times, transform=TRANSFORM, crs="SHG", variable=variable,
                          source_units=units, **kw)
    return path


def names(path):
    with dss.open(path) as f:
        return sorted(str(p) for p in f.catalog()["pathname"].tolist())


def test_feature_flag_and_exports():
    assert "gridded_time_tools" in dss.FEATURES
    for name in ("shift_times", "resample_times", "read_window", "read_nearest", "read_min_max", "series_info",
                 "time_steps"):
        assert name in gridded.__all__


def test_time_steps_are_vortex_steps():
    steps = gridded.time_steps()
    assert len(steps) == 21
    assert steps[0] == 1 and steps[-3:] == [10080, 43200, 525600]


def test_shift_moves_stamps_and_keeps_cells(tmp_path):
    source = series(tmp_path / "in.dss", first=T + 22 * HOUR)
    out = tmp_path / "out.dss"
    result = gridded.shift_times(source, out, shift=HOUR)
    assert result.pathnames == [
        "///PRECIPITATION/01JAN2024:2300/01JAN2024:2400//",
        "///PRECIPITATION/02JAN2024:0000/02JAN2024:0100//",
        "///PRECIPITATION/02JAN2024:0100/02JAN2024:0200//",
        "///PRECIPITATION/02JAN2024:0200/02JAN2024:0300//",
    ]
    assert all(r["outcome"] == "added" for r in result.records)
    with dss.open(source) as a, dss.open(out) as b:
        first = np.asarray(a.read_grid("///PRECIPITATION/01JAN2024:2200/01JAN2024:2300//", missing="raw").data)
        moved = b.read_grid("///PRECIPITATION/01JAN2024:2300/01JAN2024:2400//", missing="raw")
        assert first.tobytes() == np.asarray(moved.data).tobytes()
        assert moved.data_units == "MM"
        assert str(moved.end_time) == "2024-01-02T00:00:00"


@pytest.mark.parametrize(
    "variable, units, instants, want_type, want_name",
    [
        ("PRECIPITATION", "mm", False, 1, "///PRECIPITATION/01JAN2024:0100/01JAN2024:0200//"),
        ("TEMPERATURE", "DEG C", False, 0, "///TEMPERATURE/01JAN2024:0100/01JAN2024:0200//"),
        ("ALBEDO", "UNDEF", True, 2, "///ALBEDO/01JAN2024:0100///"),
    ],
)
def test_each_type_stamps_its_period_or_instant(tmp_path, variable, units, instants, want_type, want_name):
    source = series(tmp_path / "in.dss", frames=2, variable=variable, units=units, instants=instants)
    out = tmp_path / "out.dss"
    gridded.shift_times(source, out, shift=HOUR)
    assert want_name in names(out)
    with dss.open(out) as f:
        g = f.read_grid(want_name, missing="raw")
        assert g.data_type == want_type
        if instants:
            assert g.start_time == g.end_time or g.end_time is None or str(g.end_time) == "2024-01-01T01:00:00"
        else:
            assert str(g.start_time) == "2024-01-01T01:00:00"
            assert str(g.end_time) == "2024-01-01T02:00:00"


def test_midnight_is_0000_at_a_start_and_2400_at_an_end(tmp_path):
    source = series(tmp_path / "in.dss", frames=1)
    out = tmp_path / "out.dss"
    # Ends at 01JAN 2400, starts at 02JAN 0000, either side of midnight.
    gridded.shift_times(source, out, shift=23 * HOUR)
    assert names(out) == ["///PRECIPITATION/01JAN2024:2300/01JAN2024:2400//"]
    gridded.shift_times(source, out, shift=24 * HOUR)
    assert "///PRECIPITATION/02JAN2024:0000/02JAN2024:0100//" in names(out)
    a = series(tmp_path / "a.dss", frames=1, variable="ALBEDO", units="UNDEF", instants=True)
    b = tmp_path / "b.dss"
    gridded.shift_times(a, b, shift=24 * HOUR)
    assert names(b) == ["///ALBEDO/01JAN2024:2400///"]


def test_both_midnight_spellings_read_as_one_instant(tmp_path):
    source = series(tmp_path / "in.dss", frames=2, first=T + 23 * HOUR)
    for start in ("02JAN2024:0000", "01JAN2024:2400", dt.datetime(2024, 1, 2)):
        g = gridded.read_window(source, "///PRECIPITATION/*/*//", start, "02JAN2024:0100")
        assert g.start == "02JAN2024:0000"
        assert g.end == "02JAN2024:0100"


def test_volume_is_kept_over_complete_windows(tmp_path):
    source = series(tmp_path / "in.dss", frames=24)
    with dss.open(source) as f:
        total = sum(np.asarray(f.read_grid(p, missing="raw").data, dtype=np.float64).sum() for p in names(source))
    for minutes in (15, 60, 120, 240, 360, 720, 1440):
        out = tmp_path / f"{minutes}.dss"
        r = gridded.resample_times(source, out, minutes)
        assert len(r.records) == 1440 // minutes
        assert r.dropped_tail_seconds == 0
        with dss.open(out) as f:
            sum_out = sum(np.asarray(f.read_grid(p, missing="raw").data, dtype=np.float64).sum() for p in r.pathnames)
        # Quarters: exact in float32, so equality.
        assert sum_out == total, minutes


def test_incomplete_window_is_refused_and_stated_rule_writes(tmp_path):
    source = series(tmp_path / "in.dss", frames=6)
    gap = tmp_path / "gap.dss"
    dss.create(gap)
    with dss.open(source) as f:
        keep = [p for p in names(source) if "0300/" not in p.split("PRECIPITATION")[1][:16]]
    r = gridded.shift_times(source, gap, shift=dt.timedelta(minutes=0, hours=24), select=keep)
    assert len(r.records) == 5
    out = tmp_path / "out.dss"
    with pytest.raises(exceptions.DssUnsupportedRecordError, match="has no source") as e:
        gridded.resample_times(gap, out, 1440 // 4)
    assert e.value.phase == "planning"
    assert not out.exists()
    written = gridded.resample_times(gap, out, 360, rule="vortex")
    assert written.pathnames == ["///PRECIPITATION/02JAN2024:0000/02JAN2024:0600//"]


def test_leading_and_trailing_windows_are_stated(tmp_path):
    source = series(tmp_path / "in.dss", frames=5, first=T + 7 * HOUR)
    out = tmp_path / "out.dss"
    r = gridded.resample_times(source, out, 180)
    assert r.pathnames[0] == "///PRECIPITATION/01JAN2024:0000/01JAN2024:0300//"
    assert r.windows_every_cell_missing == 3
    assert r.dropped_tail_seconds == 0
    with dss.open(out) as f:
        data = np.asarray(f.read_grid(r.pathnames[0], missing="raw").data)
    assert np.all(data == MARKER)
    assert [x["every_cell_missing"] for x in r.records] == [True, True, True, False]
    tail = series(tmp_path / "t.dss", frames=4, first=T + 7 * HOUR, step=dt.timedelta(minutes=30))
    r = gridded.resample_times(tail, tmp_path / "t2.dss", 60)
    assert r.dropped_tail_seconds == 0
    r = gridded.resample_times(tail, tmp_path / "t3.dss", 120)
    assert r.dropped_tail_seconds == 3600


@pytest.mark.parametrize(
    "kw, match",
    [
        (dict(shift=dt.timedelta(seconds=30)), "whole minute"),
        (dict(shift_start=HOUR, shift_end=dt.timedelta(0)), "precipitation at an instant"),
        (dict(shift_end=-2 * HOUR), "before it starts"),
        (dict(shift=HOUR, select=["///NOPE/01JAN2024:0000/01JAN2024:0100//"]), "not a grid record"),
        (dict(shift=HOUR, parts={"A": "*"}), "cannot be '\\*'"),
        (dict(shift=HOUR, parts={"B": "*"}), "cannot be '\\*'"),
    ],
)
def test_refusals_are_typed_and_leave_no_file(tmp_path, kw, match):
    source = series(tmp_path / "in.dss", frames=2)
    out = tmp_path / "out.dss"
    with pytest.raises(exceptions.DssUnsupportedRecordError, match=match):
        gridded.shift_times(source, out, **kw)
    assert not out.exists()


@pytest.mark.parametrize(
    "kw",
    [
        dict(),
        dict(shift=dt.timedelta(0)),
        dict(shift=HOUR, shift_end=HOUR),
        dict(shift=3600),
        dict(shift=dt.timedelta(seconds=0.5)),
        dict(shift=HOUR, version=5),
        dict(shift=HOUR, select=[]),
        dict(shift=HOUR, parts={"D": "x"}),
        dict(shift=HOUR, parts={"A": 1}),
        dict(shift=HOUR, data_type="OTHER"),
    ],
)
def test_bad_arguments_raise_value_error(tmp_path, kw):
    source = series(tmp_path / "in.dss", frames=1)
    with pytest.raises(ValueError):
        gridded.shift_times(source, tmp_path / "out.dss", **kw)
    assert not (tmp_path / "out.dss").exists()


@pytest.mark.parametrize("step", [7, 0, -60, True, 1.5, "60"])
def test_bad_steps_are_refused(tmp_path, step):
    source = series(tmp_path / "in.dss", frames=3)
    with pytest.raises((ValueError, exceptions.DssUnsupportedRecordError)):
        gridded.resample_times(source, tmp_path / "out.dss", step)
    assert not (tmp_path / "out.dss").exists()


def test_the_rule_must_be_known(tmp_path):
    source = series(tmp_path / "in.dss", frames=3)
    with pytest.raises(ValueError, match="rule"):
        gridded.resample_times(source, tmp_path / "out.dss", 60, rule="lenient")


def test_no_complete_window_is_refused(tmp_path):
    source = series(tmp_path / "in.dss", frames=3)
    with pytest.raises(exceptions.DssUnsupportedRecordError, match="no complete window"):
        gridded.resample_times(source, tmp_path / "out.dss", 1440)
    assert not (tmp_path / "out.dss").exists()


def test_input_cannot_be_output_and_output_keeps_its_records(tmp_path):
    source = series(tmp_path / "in.dss", frames=2)
    before = source.read_bytes()
    with pytest.raises(exceptions.DssUnsupportedRecordError, match="it is also the input"):
        gridded.shift_times(source, source, shift=HOUR)
    assert source.read_bytes() == before
    out = series(tmp_path / "out.dss", frames=1, variable="TEMPERATURE", units="DEG C")
    gridded.shift_times(source, out, shift=HOUR)
    assert any("TEMPERATURE" in p for p in names(out))
    assert sum("PRECIPITATION" in p for p in names(out)) == 2


def test_parts_as_stated_and_source_f_kept(tmp_path):
    source = series(tmp_path / "in.dss", frames=1, parts={"A": "A1", "B": "B1", "F": "F1"})
    out = tmp_path / "a.dss"
    gridded.shift_times(source, out, shift=HOUR)
    assert names(out) == ["///PRECIPITATION/01JAN2024:0100/01JAN2024:0200//"], "A and B are not kept, as in Vortex"
    out = tmp_path / "b.dss"
    gridded.shift_times(source, out, shift=HOUR, parts={"A": "X", "B": "Y", "F": "*"})
    assert names(out) == ["/X/Y/PRECIPITATION/01JAN2024:0100/01JAN2024:0200/F1/"]


def test_read_window_maps_missing_only_when_asked(tmp_path):
    data = np.ones((1, *SHAPE), dtype=np.float32)
    data[0, 0, 0] = MARKER
    source = tmp_path / "in.dss"
    gridded.import_arrays(data, source, times=[(T, T + HOUR)], transform=TRANSFORM, crs="SHG",
                          variable="PRECIPITATION", source_units="mm", nodata=float(MARKER), parts={})
    g = gridded.read_window(source, "///PRECIPITATION/*/*//", "01JAN2024:0000", "01JAN2024:0100")
    assert g.data.shape == SHAPE
    # South to north, as DSS stores: the frame's first row is north.
    assert np.isnan(g.data[-1, 0])
    raw = gridded.read_window(source, "///PRECIPITATION/*/*//", "01JAN2024:0000", "01JAN2024:0100", missing="raw")
    assert raw.data[-1, 0] == MARKER
    with pytest.raises(ValueError, match="missing"):
        gridded.read_window(source, "///PRECIPITATION/*/*//", "01JAN2024:0000", "01JAN2024:0100", missing="zero")


def test_reads_refuse_a_series_with_no_rule_or_no_records(tmp_path):
    source = series(tmp_path / "in.dss", frames=2)
    with pytest.raises(exceptions.DssUnsupportedRecordError, match="no grid record"):
        gridded.series_info(source, "///NOPE/*/*//")
    with pytest.raises(exceptions.DssUnsupportedRecordError, match="at an instant"):
        gridded.read_window(source, "///PRECIPITATION/*/*//", "01JAN2024:0100")
    with pytest.raises(ValueError, match="not a time"):
        gridded.read_window(source, "///PRECIPITATION/*/*//", "soon")
    with pytest.raises(ValueError, match="naive"):
        gridded.read_window(source, "///PRECIPITATION/*/*//", dt.datetime(2024, 1, 1, tzinfo=dt.timezone.utc))
    with pytest.raises(ValueError, match="whole minute"):
        gridded.read_window(source, "///PRECIPITATION/*/*//", dt.datetime(2024, 1, 1, 0, 0, 30))


def test_series_info_nearest_and_min_max(tmp_path):
    source = series(tmp_path / "in.dss", frames=3, first=T + 22 * HOUR)
    i = gridded.series_info(source, "///PRECIPITATION/*/*//")
    assert i["records"] == 3
    assert (i["start"], i["end"]) == ("01JAN2024:2200", "02JAN2024:0100")
    assert i["units"] == "MM" and i["data_type"] == "PER-CUM"
    g = gridded.read_nearest(source, "///PRECIPITATION/*/*//", "01JAN2024:2300")
    assert (g.start, g.end) == ("01JAN2024:2300", "01JAN2024:2400")
    lo, hi = gridded.read_min_max(source, "///PRECIPITATION/*/*//", "01JAN2024:2200", "02JAN2024:0100")
    assert lo.data.min() == 0.0
    assert hi.data.max() == pytest.approx((5 + 20) * 0.25)


def test_a_callback_can_stop_and_its_exception_is_raised_as_itself(tmp_path):
    source = series(tmp_path / "in.dss", frames=3)
    out = tmp_path / "out.dss"

    def boom(done, total):
        raise KeyError("stop")

    with pytest.raises(KeyError) as e:
        gridded.shift_times(source, out, shift=HOUR, progress=boom)
    assert len(e.value.completed) == 1
    seen = []
    out = tmp_path / "out2.dss"
    r = gridded.shift_times(source, out, shift=HOUR, progress=lambda d, t: seen.append((d, t)))
    assert seen == [(1, 3), (2, 3), (3, 3)] and len(r.records) == 3
    with pytest.raises(exceptions.DssError, match="cancelled") as e:
        gridded.shift_times(source, tmp_path / "out3.dss", shift=HOUR, progress=lambda d, t: False)
    assert len(e.value.completed) == 1


def test_a_failure_in_the_last_callback_is_not_lost(tmp_path):
    source = series(tmp_path / "in.dss", frames=2)

    def on_last(done, total):
        if done == total:
            raise KeyError("last")

    with pytest.raises(KeyError) as e:
        gridded.shift_times(source, tmp_path / "out.dss", shift=HOUR, progress=on_last)
    assert len(e.value.completed) == 2


def test_the_gil_is_released(tmp_path):
    source = series(tmp_path / "in.dss", frames=48)
    ticks = []
    stop = threading.Event()

    def spin():
        while not stop.is_set():
            ticks.append(1)
            threading.Event().wait(0.001)

    th = threading.Thread(target=spin)
    th.start()
    try:
        gridded.resample_times(source, tmp_path / "out.dss", 15)
    finally:
        stop.set()
        th.join()
    assert ticks


def test_no_java_is_loaded():
    import sys

    assert not any(m == "jnius" or m.startswith("jnius.") or m == "jpype" for m in sys.modules)


def test_dss6_output(tmp_path):
    source = series(tmp_path / "in.dss", frames=2)
    out = tmp_path / "six.dss"
    gridded.shift_times(source, out, shift=HOUR, version=6, parts={"A": "A", "B": "B", "F": "F"})
    with dss.open(out) as f:
        assert int(f.version) == 6
        assert "/A/B/PRECIPITATION/01JAN2024:0100/01JAN2024:0200/F/" in [str(p) for p in f.catalog()["pathname"].tolist()]
    back = tmp_path / "back.dss"
    r = gridded.resample_times(out, back, 60, parts={"A": "A", "B": "B", "F": "F"})
    # 00-01 begins before the series, which starts at 01:00: missing, and stated.
    assert len(r.records) == 3
    assert r.windows_every_cell_missing == 1


# ---- values, files and CRS --------------------------------------------------------------------------


def albers_file(path, units, **over):
    albers = dict(first_standard_parallel=29.5, second_standard_parallel=45.5, central_meridian=-96.0,
                  latitude_of_origin=23.0, false_easting=0.0, false_northing=0.0, datum=2, units=units)
    albers.update(over)
    dss.create(path)
    for i in range(2):
        dss.write_grid(path, f"///PRECIPITATION/01JAN2024:0{i}00/01JAN2024:0{i + 1}00//",
                       np.full(SHAPE, 1.5, dtype=np.float32), grid_type="albers", cell_size=2000.0,
                       lower_left_cell=(1, 2), compression="zlib", units="MM", data_type="PER-CUM", albers=albers)
    return path


def raw_series(path, values):
    dss.create(path)
    for i, value in enumerate(values):
        data = np.arange(6, dtype=np.float32).reshape(SHAPE) * 0.5
        data[0, 0] = value
        dss.write_grid(path, f"///PRECIPITATION/01JAN2024:0{i}00/01JAN2024:0{i + 1}00//", data, grid_type="albers",
                       cell_size=2000.0, lower_left_cell=(1, 2), compression="zlib", units="MM", data_type="PER-CUM",
                       raw=True, albers=dict(first_standard_parallel=29.5, second_standard_parallel=45.5,
                                             central_meridian=-96.0, latitude_of_origin=23.0, false_easting=0.0,
                                             false_northing=0.0, datum=2, units="METERS"))
    return path


@pytest.mark.parametrize("units", ["FEET", "US FOOT", "Foot_US", "km", "", "miles"])
@pytest.mark.parametrize("other_crs", [False, True])
def test_an_albers_grid_not_in_metres_is_refused(tmp_path, units, other_crs):
    over = {"central_meridian": -100.0} if other_crs else {}
    source = albers_file(tmp_path / "in.dss", units, **over)
    out = tmp_path / "out.dss"
    with pytest.raises(exceptions.DssUnsupportedRecordError, match="Albers grid"):
        gridded.shift_times(source, out, shift=HOUR)
    with pytest.raises(exceptions.DssUnsupportedRecordError, match="Albers grid"):
        gridded.resample_times(source, out, 60)
    assert not out.exists()


@pytest.mark.parametrize("units", ["METERS", "meters", "M", "m", "meter", "metre", "METRES"])
def test_an_albers_grid_stating_metres_is_shg_when_its_parameters_are(tmp_path, units):
    out = tmp_path / "out.dss"
    gridded.shift_times(albers_file(tmp_path / "in.dss", units), out, shift=HOUR)
    with dss.open(out) as f:
        g = f.read_grid("///PRECIPITATION/01JAN2024:0100/01JAN2024:0200//")
    assert "albers" in str(g.grid_type).lower()


def test_other_albers_parameters_in_metres_become_a_specified_grid_that_says_metres(tmp_path):
    out = tmp_path / "out.dss"
    gridded.shift_times(albers_file(tmp_path / "in.dss", "METERS", central_meridian=-100.0), out, shift=HOUR)
    with dss.open(out) as f:
        g = f.read_grid("///PRECIPITATION/01JAN2024:0100/01JAN2024:0200//")
    assert 'UNIT["Meter",1]' in g.srs_definition
    assert "Foot" not in g.srs_definition


@pytest.mark.parametrize("value", [-901.0, -902.0])
def test_a_value_the_reader_would_lose_is_refused_by_record_and_time(tmp_path, value):
    source = raw_series(tmp_path / "in.dss", [0.5, value])
    out = tmp_path / "out.dss"
    with pytest.raises(exceptions.DssUnsupportedRecordError, match="take as missing") as e:
        gridded.shift_times(source, out, shift=HOUR)
    assert "///PRECIPITATION/01JAN2024:0200/01JAN2024:0300//" in str(e.value)
    assert "2024-01-01T02:00 to 2024-01-01T03:00" in str(e.value)
    assert not out.exists()
    # An existing output keeps its bytes.
    kept = series(tmp_path / "kept.dss", frames=1, variable="TEMPERATURE", units="DEG C")
    before = kept.read_bytes()
    with pytest.raises(exceptions.DssUnsupportedRecordError, match="take as missing"):
        gridded.shift_times(source, kept, shift=HOUR)
    assert kept.read_bytes() == before


def test_an_accumulation_landing_on_it_is_refused_and_nothing_is_written(tmp_path):
    source = raw_series(tmp_path / "in.dss", [1.0, 2.0, -450.5, -450.5])
    out = tmp_path / "out.dss"
    for rule in ("strict", "vortex"):
        with pytest.raises(exceptions.DssUnsupportedRecordError, match="take as missing") as e:
            gridded.resample_times(source, out, 120, rule=rule)
        assert "2024-01-01T02:00 to 2024-01-01T04:00" in str(e.value)
        assert e.value.completed in (None, [])
        assert e.value.in_doubt is None
        assert not out.exists()


def test_a_source_the_reader_calls_missing_is_refused_strictly_and_a_number_otherwise(tmp_path):
    source = raw_series(tmp_path / "in.dss", [1.0, -901.0])
    with pytest.raises(exceptions.DssUnsupportedRecordError, match="takes as missing"):
        gridded.resample_times(source, tmp_path / "s.dss", 120)
    out = tmp_path / "v.dss"
    gridded.resample_times(source, out, 120, rule="vortex")
    with dss.open(out) as f:
        data = f.read_grid("///PRECIPITATION/01JAN2024:0000/01JAN2024:0200//", missing="raw").data
    assert data.reshape(-1)[0] == -900.0


def test_a_hard_link_to_the_input_is_refused_and_both_names_keep_their_bytes(tmp_path):
    source = series(tmp_path / "in.dss", frames=2)
    link = tmp_path / "alias.dss"
    import os

    os.link(source, link)
    before = source.read_bytes()
    with pytest.raises(exceptions.DssUnsupportedRecordError, match="it is also the input"):
        gridded.shift_times(source, link, shift=HOUR)
    with pytest.raises(exceptions.DssUnsupportedRecordError, match="it is also the input"):
        gridded.resample_times(source, link, 60)
    assert source.read_bytes() == before
    assert link.read_bytes() == before


def test_a_refusal_states_nothing_in_doubt(tmp_path):
    source = series(tmp_path / "in.dss", frames=2)
    with pytest.raises(exceptions.DssUnsupportedRecordError) as e:
        gridded.shift_times(source, tmp_path / "o.dss", shift=dt.timedelta(seconds=30))
    assert e.value.in_doubt is None


def test_a_rerun_leaves_records_outside_its_windows(tmp_path):
    long_series = series(tmp_path / "long.dss", frames=4)
    short = tmp_path / "short.dss"
    data = np.full((2, *SHAPE), 9.5, dtype=np.float32)
    gridded.import_arrays(data, short, times=[(T + i * HOUR, T + (i + 1) * HOUR) for i in range(2)], transform=TRANSFORM,
                          crs="SHG", variable="PRECIPITATION", source_units="mm", parts={})
    out = tmp_path / "out.dss"
    gridded.resample_times(long_series, out, 60)
    with dss.open(out) as f:
        before = {n: f.read_grid(n, missing="raw").data.copy() for n in names(out)}
    result = gridded.resample_times(short, out, 60)
    assert [r["outcome"] for r in result.records] == ["replaced", "replaced"]
    with dss.open(out) as f:
        after = {n: f.read_grid(n, missing="raw").data.copy() for n in names(out)}
    assert set(after) == set(before)
    for name in result.pathnames:
        assert (after[name] == 9.5).all()
    for name in set(after) - set(result.pathnames):
        assert after[name].tobytes() == before[name].tobytes()


# ---- HRAP, NAD27, and what stays refused ----------------------------------------------------------------------

ODD = dict(first_standard_parallel=29.3, second_standard_parallel=45.7, central_meridian=-100.123, latitude_of_origin=23.1,
           false_easting=500000.5, false_northing=-1234.75)


def crs_series(path, frames=4, **grid):
    dss.create(path)
    for i in range(frames):
        data = (np.arange(6, dtype=np.float32).reshape(SHAPE) + 10 * i + 1) * 0.25
        dss.write_grid(path, f"///PRECIPITATION/01JAN2024:0{i}00/01JAN2024:0{i + 1}00//", data, units="MM", data_type="PER-CUM",
                       compression="zlib", **grid)
    return path


HRAP = dict(grid_type="hrap", cell_size=4762.5, lower_left_cell=(300, 400))
NAD27 = dict(grid_type="albers", cell_size=2000.0, lower_left_cell=(1, 2), albers=dict(ODD, datum=1, units="METERS"))


@pytest.mark.parametrize("version", [7, 6])
@pytest.mark.parametrize("grid,words", [(HRAP, "Stereographic_CONUS_HRAP"), (NAD27, 'GEOGCS["NAD27"')], ids=["hrap", "nad27"])
def test_hrap_and_nad27_are_shifted_and_resampled(tmp_path, grid, words, version):
    source = crs_series(tmp_path / "in.dss", **grid)
    with dss.open(source) as f:
        first = f.read_grid("///PRECIPITATION/01JAN2024:0000/01JAN2024:0100//", missing="raw")
    shifted = tmp_path / "shift.dss"
    gridded.shift_times(source, shifted, shift=HOUR, version=version)
    with dss.open(shifted) as f:
        g = f.read_grid("///PRECIPITATION/01JAN2024:0100/01JAN2024:0200//", missing="raw")
    assert words in g.srs_definition
    assert g.data.tobytes() == first.data.tobytes()
    assert (g.lower_left_cell_x, g.lower_left_cell_y, g.cell_size) == (first.lower_left_cell_x, first.lower_left_cell_y, first.cell_size)
    resampled = tmp_path / "resample.dss"
    gridded.resample_times(source, resampled, 120, version=version)
    with dss.open(resampled) as f:
        g = f.read_grid("///PRECIPITATION/01JAN2024:0000/01JAN2024:0200//", missing="raw")
    assert words in g.srs_definition
    assert g.data[0, 0] == pytest.approx(0.25 + 2.75)


def test_a_time_tool_words_albers_angles_as_vortex_stores_them(tmp_path):
    grid = dict(grid_type="albers", cell_size=2000.0, lower_left_cell=(1, 2), albers=dict(ODD, datum=2, units="METERS"))
    out = tmp_path / "out.dss"
    gridded.shift_times(crs_series(tmp_path / "in.dss", **grid), out, shift=HOUR)
    with dss.open(out) as f:
        text = f.read_grid("///PRECIPITATION/01JAN2024:0100/01JAN2024:0200//").srs_definition
    assert 'PARAMETER["standard_parallel_1",29.2999992370606]' in text


@pytest.mark.parametrize(
    "grid,match",
    [
        (dict(grid_type="albers", cell_size=2000.0, lower_left_cell=(1, 2), albers=dict(ODD, datum=0, units="METERS")), "states no datum"),
        (dict(grid_type="albers", cell_size=2000.0, lower_left_cell=(1, 2), albers=dict(ODD, datum=2, units="METERS"),
              cell_zero=(2000.0, 0.0)), "cell zero"),
    ],
    ids=["no_datum", "cell_zero"],
)
def test_what_states_no_usable_crs_stays_refused(tmp_path, grid, match):
    source = crs_series(tmp_path / "in.dss", **grid)
    for run in (lambda o: gridded.shift_times(source, o, shift=HOUR), lambda o: gridded.resample_times(source, o, 60)):
        out = tmp_path / "out.dss"
        with pytest.raises(exceptions.DssUnsupportedRecordError, match=match):
            run(out)
        assert not out.exists()


def test_transverse_mercator_is_read_and_kept_as_stated(tmp_path):
    # Refused until the CRS module held Transverse Mercator. The projection itself is
    # checked against PROJ 7.2.0 as Vortex 1.1.0 runs it, in
    # crates/dss-vortex/tests/crs_tm.rs.
    wkt = ('PROJCS["NAD83 / UTM zone 11N",GEOGCS["NAD83",DATUM["North_American_Datum_1983",SPHEROID["GRS 1980",6378137,298.257222101]],'
           'PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["latitude_of_origin",0],'
           'PARAMETER["central_meridian",-117],PARAMETER["scale_factor",0.9996],PARAMETER["false_easting",500000],'
           'PARAMETER["false_northing",0],UNIT["metre",1]]')
    source = crs_series(tmp_path / "in.dss", grid_type="specified", cell_size=1000.0, lower_left_cell=(300, 4000),
                        srs_definition=wkt, srs_name="NAD83 / UTM zone 11N")
    out = tmp_path / "out.dss"
    result = gridded.shift_times(source, out, shift=HOUR)
    assert len(result.pathnames) >= 1
    with dss.open(source) as a, dss.open(out) as b:
        first = np.asarray(a.read_grid("///PRECIPITATION/01JAN2024:0000/01JAN2024:0100//", missing="raw").data)
        moved = np.asarray(b.read_grid("///PRECIPITATION/01JAN2024:0100/01JAN2024:0200//", missing="raw").data)
    assert first.tobytes() == moved.tobytes()


def test_a_specified_grids_own_null_value_is_refused_not_used_as_a_number(tmp_path):
    wkt = ('GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],'
           'UNIT["degree",0.0174532925199433]]')
    source = tmp_path / "in.dss"
    dss.create(source)
    for i in range(2):
        data = np.full(SHAPE, 1.5, dtype=np.float32)
        data[0, 0] = -9999.0
        dss.write_grid(source, f"///PRECIPITATION/01JAN2024:0{i}00/01JAN2024:0{i + 1}00//", data, units="MM", data_type="PER-CUM",
                       grid_type="specified", cell_size=0.01, lower_left_cell=(1, 2), compression="zlib", srs_definition=wkt,
                       srs_name="WGS 84", raw=True, null_value=-9999.0)
    out = tmp_path / "out.dss"
    with pytest.raises(exceptions.DssUnsupportedRecordError, match="null value"):
        gridded.shift_times(source, out, shift=HOUR)
    with pytest.raises(exceptions.DssUnsupportedRecordError, match="null value"):
        gridded.resample_times(source, out, 120)
    assert not out.exists()
    gridded.resample_times(source, out, 120, rule="vortex")
    with dss.open(out) as f:
        data = f.read_grid("///PRECIPITATION/01JAN2024:0000/01JAN2024:0200//", missing="raw").data
    assert data[0, 0] == -19998.0, "Vortex's rule: a number, as there"


def test_an_input_changed_during_a_run_stops_it_with_what_was_written(tmp_path):
    source = series(tmp_path / "in.dss", frames=4)
    out = tmp_path / "out.dss"
    def change(done, total):
        if done == 1:
            series(source, frames=1, first=T + 100 * HOUR)
    with pytest.raises(exceptions.DssUnsupportedRecordError, match="changed since") as e:
        gridded.shift_times(source, out, shift=HOUR, progress=change)
    assert len(e.value.completed) == 1
    assert e.value.in_doubt is None

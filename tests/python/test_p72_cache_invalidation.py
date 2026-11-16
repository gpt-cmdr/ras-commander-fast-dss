"""A write through a ``DssFile`` drops every cache the object derived from
the file's contents, so reads through the same object see the write.

The only such cache is the DSS6 dataset-location index (``_p15_ts_index``, built by
``read_associated_location``'s D-part fallback). Up to 0.1.1, writes cleared a misspelled
attribute and the index survived, so a dataset-level location read through the same object
kept answering from the catalog it saw before the write. Synthetic files only.
"""

from __future__ import annotations

import numpy as np
import pytest

import ras_commander_fast_dss as dss

SERIES = "/R/B/FLOW//1HOUR/S/"


def _prime_empty_index(f):
    with pytest.raises(dss.DssPathnameNotFoundError):
        f.read_associated_location(SERIES)
    assert f.__dict__.get("_p15_ts_index") is not None  # the index is now cached


def test_dss6_location_read_after_write_sees_the_write(tmp_path):
    p = tmp_path / "cache.dss"
    dss.create(p, version=6)
    with dss.open(p) as f:
        _prime_empty_index(f)
        f.write_timeseries(SERIES, [1.0, 2.0], start="01JAN2021 0100", units="CFS",
                           data_type="INST-VAL", sync=False)
        f.write_location(SERIES, x=10.0, y=20.0, z=30.0, sync=False)
        loc = f.read_associated_location(SERIES)
        assert tuple(loc.coordinates)[:3] == (10.0, 20.0, 30.0)
    with dss.open(p) as f:
        assert tuple(f.read_associated_location(SERIES).coordinates)[:3] == (10.0, 20.0, 30.0)


def _write(f, kind):
    if kind == "timeseries":
        f.write_timeseries("/R/X/FLOW//1HOUR/S/", [1.0], start="01JAN2021 0100", units="CFS",
                           data_type="INST-VAL", sync=False)
    elif kind == "paired_data":
        f.write_paired_data("/R/X/FREQ-FLOW///S/", [1.0, 2.0], [3.0, 4.0], x_units="FT",
                            x_type="LINEAR", y_units="CFS", y_type="LINEAR", sync=False)
    elif kind == "grid":
        f.write_grid("/SHG/X/PRECIP/01SEP2018:0100/01SEP2018:0200/S/",
                     np.ones((2, 3), dtype=np.float32), grid_type="shg", cell_size=2000.0,
                     units="MM", data_type="PER-CUM", lower_left_cell=(-1000, 700), sync=False)
    elif kind == "location":
        f.write_location("/R/X/FLOW//1HOUR/S/", x=1.0, y=2.0, z=3.0, sync=False)
    else:
        raise AssertionError(kind)


@pytest.mark.parametrize("kind", ["timeseries", "paired_data", "grid", "location"])
def test_every_dssfile_writer_drops_the_index(tmp_path, kind):
    p = tmp_path / f"{kind}.dss"
    dss.create(p, version=6)
    if kind == "location":
        # A DSS6 location is stored in the series' own records: write the series first,
        # not through the object under test.
        dss.write_timeseries(p, "/R/X/FLOW//1HOUR/S/", [1.0], start="01JAN2021 0100",
                             units="CFS", data_type="INST-VAL", version=6, sync=False)
    with dss.open(p) as f:
        _prime_empty_index(f)
        _write(f, kind)
        assert "_p15_ts_index" not in f.__dict__


def test_profile_writer_drops_the_index(tmp_path):
    # Profiles are DSS7 only; the index is only consulted for DSS6, but the writer must
    # still not leave a cache built before the write.
    p = tmp_path / "profile.dss"
    dss.create(p, version=7)
    with dss.open(p) as f:
        f.__dict__["_p15_ts_index"] = {}
        f.write_profile("/R/P/DEPTH-TEMP/01JAN2021/1HOUR/S/", [0.0, 1.0],
                        [[1.0, 2.0], [3.0, 4.0]], start="01JAN2021 0100", depth_units="FT",
                        value_units="DEG C", data_type="INST-VAL", sync=False)
        assert "_p15_ts_index" not in f.__dict__


def test_a_failed_write_also_drops_the_index(tmp_path):
    p = tmp_path / "failed.dss"
    dss.create(p, version=6)
    with dss.open(p) as f:
        _prime_empty_index(f)
        with pytest.raises((ValueError, TypeError)):
            f.write_timeseries(SERIES, [1.0], start="01JAN2021 0100", units="CFS",
                               data_type="INST-VAL", sync="sometimes")
        assert "_p15_ts_index" not in f.__dict__


# -- a read made while write_batch consumes its records --------------------------------
#
# `_begin_write` drops the index before the batch consumes `records`. A generator that
# reads a dataset location while it is consumed rebuilds the index from the file as it
# was before the native batch writes anything; the session must drop it again on exit,
# including when the batch fails part way. Before the fix the rebuilt index survived, so a
# later dataset-level lookup skipped the new blocks.

OLD = "/CACHE/OLD/FLOW//1HOUR/T/"
NEW = "/CACHE/NEW/FLOW//1HOUR/T/"
TS6 = dict(units="CFS", data_type="INST-VAL")


def _located_dss6(tmp_path):
    p = tmp_path / "batch-cache.dss"
    dss.create(p, version=6)
    dss.write_timeseries(p, OLD, [1.0, 2.0], start="2020-01-01 01:00", version=6, **TS6)
    dss.write_location(p, OLD, x=1.0, y=2.0, coordinate_system=1)
    return p


def _reading(f, *recs):
    """Records for ``f.write_batch`` that read ``OLD``'s location (caching the index)
    before yielding."""
    assert tuple(f.read_associated_location(OLD).coordinates)[:2] == (1.0, 2.0)
    assert f.__dict__.get("_p15_ts_index") is not None
    yield from recs


def test_batch_generator_read_then_new_dataset(tmp_path):
    p = _located_dss6(tmp_path)
    with dss.open(p) as f:
        f.write_batch(_reading(f, dict(kind="timeseries", pathname=NEW, values=[3.0, 4.0],
                                       start="2020-01-01 01:00", **TS6)))
        # The new dataset has no location: None, not "missing".
        assert f.read_associated_location(NEW) is None
        assert list(f.read_timeseries(NEW)["value"]) == [3.0, 4.0]
    with dss.open(p) as f:
        assert f.read_associated_location(NEW) is None


def test_batch_generator_read_then_new_block_conflicts(tmp_path):
    p = _located_dss6(tmp_path)
    with dss.open(p) as f:
        f.write_batch(_reading(f, dict(kind="timeseries", pathname=OLD, values=[5.0, 6.0],
                                       start="2021-01-01 01:00", **TS6)))
        # The 2021 block stores no coordinates and the 2020 block does: the dataset-level
        # lookup must see both and refuse, not return the 2020 block's location.
        with pytest.raises(dss.DssLocationConflictError):
            f.read_associated_location(OLD)
    with dss.open(p) as f:
        with pytest.raises(dss.DssLocationConflictError):
            f.read_associated_location(OLD)


def test_batch_generator_read_then_partial_failure(tmp_path):
    p = _located_dss6(tmp_path)
    with dss.open(p) as f:
        recs = _reading(
            f,
            dict(kind="timeseries", pathname=OLD, values=[5.0, 6.0],
                 start="2021-01-01 01:00", **TS6),
            # Paired data onto the stored 2020 time-series block is refused while the batch
            # is written, after the 2021 block is stored.
            dict(kind="paired_data", pathname="/CACHE/OLD/FLOW/01JAN2020/1HOUR/T/",
                 x=[0.0, 1.0], y=[1.0, 2.0], x_units="FT", x_type="LINEAR", y_units="CFS",
                 y_type="LINEAR"),
        )
        with pytest.raises(dss.DssUnsupportedRecordError) as err:
            f.write_batch(recs)
        assert err.value.batch_index == 1
        assert len(err.value.batch_reports) == 1
        with pytest.raises(dss.DssLocationConflictError):
            f.read_associated_location(OLD)

"""Writer snapshots and curve ordering across array layouts and widths."""

import numpy as np
import pytest

import ras_commander_fast_dss as dss
from ras_commander_fast_dss._write import _values_array
from ras_commander_fast_dss._write_pd import _cast


@pytest.mark.parametrize("dtype", [np.float32, np.float64])
@pytest.mark.parametrize("paired", [False, True])
def test_raw_snapshot_preserves_bits_and_does_not_alias(dtype, paired):
    uint = np.uint32 if dtype == np.float32 else np.uint64
    bits = ([0x80000000, 0x7FC12345, 0x7F800000] if dtype == np.float32 else
            [0x8000000000000000, 0x7FF8123456789ABC, 0x7FF0000000000000])
    source = np.array(bits, dtype=uint).view(dtype)[::-1]
    before = source.tobytes()
    prepared = (_cast(source, dtype, "y", True, -901.0) if paired else
                _values_array(source, "auto", True))
    assert prepared.flags.c_contiguous
    assert prepared.tobytes() == before
    assert not np.shares_memory(source, prepared)
    assert source.tobytes() == before


@pytest.mark.parametrize("version", [6, 7])
@pytest.mark.parametrize("dtype", [np.float32, np.float64])
@pytest.mark.parametrize("order", ["C", "F"])
def test_batch_snapshots_reused_buffers_and_preserves_curve_order(tmp_path, version, dtype, order):
    path = tmp_path / "snapshots.dss"
    dss.create(path, version=version, sync="none")
    ts = np.array([4.0, np.nan, 2.0, -0.0], dtype=dtype)[::-1]
    x = np.arange(8, dtype=dtype)[::2]
    y = np.array([[1, 10], [2, 20], [3, 30], [4, 40]], dtype=dtype, order=order)
    expected_ts, expected_x, expected_y = ts.copy(), x.copy(), y.copy()
    ts_path, pd_path = "/P/S/FLOW//1HOUR/F/", "/P/S/STAGE-FLOW///F/"

    def records():
        yield dict(kind="timeseries", pathname=ts_path, values=ts,
                   start="01JAN2021 01:00", units="CFS", data_type="INST-VAL")
        # Collection must already have snapshotted the previous record.
        ts[:] = 99
        yield dict(kind="paired_data", pathname=pd_path, x=x, y=y,
                   x_units="FT", x_type="LINEAR", y_units="CFS", y_type="LINEAR")
        x[:] = 88
        y[:] = 77

    reports = dss.write_batch(path, records(), version=version, sync="none")
    assert len(reports) == 2
    actual_ts = dss.read_timeseries(path, ts_path)["value"].to_numpy()
    np.testing.assert_array_equal(actual_ts, expected_ts)
    assert np.signbit(actual_ts[0])
    actual_pd = dss.read_paired_data(path, pd_path)
    np.testing.assert_array_equal(actual_pd.ordinates, expected_x)
    np.testing.assert_array_equal(actual_pd.values, expected_y.T)
    assert actual_pd.ordinates.dtype == dtype
    assert actual_pd.values.dtype == dtype


@pytest.mark.parametrize("version", [6, 7])
def test_transposed_curve_overflow_reports_flat_curve_index_before_writing(tmp_path, version):
    path = tmp_path / "overflow.dss"
    dss.create(path, version=version, sync="none")
    before = path.read_bytes()
    y = np.array([[1.0, 2.0], [3.0, 1e100]])
    with pytest.raises(ValueError, match=r"y\[3\].*overflows"):
        dss.write_paired_data(path, "/P/S/STAGE-FLOW///F/", [0.0, 1.0], y,
                              x_units="FT", x_type="LINEAR", y_units="CFS",
                              y_type="LINEAR", value_type="float", version=version)
    assert path.read_bytes() == before

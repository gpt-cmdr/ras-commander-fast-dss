"""P23: paired-data writer (DSS7 and DSS6) through the public Python API.

Synthetic data only; every file is created in a temporary directory. These tests do not
run HEC software (heclib zpdStore, hecdss, Monolith HecPairedData).
"""

import numpy as np
import pandas as pd
import pytest

import ras_commander_fast_dss as dss
from ras_commander_fast_dss.exceptions import DssUnsupportedRecordError

UNDEF32 = np.float32(-np.finfo(np.float32).max)


@pytest.fixture(params=[7, 6])
def version(request):
    return request.param


def _new(tmp_path, version, name="pd.dss"):
    p = tmp_path / name
    dss.create(p, version=version)
    return p


def test_multi_curve_labels_float32(tmp_path, version):
    p = _new(tmp_path, version)
    x = np.array([0.0, 2.0, 4.0, 6.0], dtype=np.float32)
    y = np.array([[1, 10, 100], [2, 20, 200], [3, 30, 300], [4, 40, 400]], dtype=np.float32)
    rep = dss.write_paired_data(
        p, "/BASIN/LOC/STAGE-DAMAGE///PLAN A/", x, y,
        x_units="FT", x_type="LINEAR", y_units="DOLLARS", y_type="LINEAR",
        labels=["Total", "Residential structures", "Commercial"],
        x_precision=1, y_precision=0, version=version,
    )
    assert rep["blocks"][0]["outcome"] == "added"
    assert rep["blocks"][0]["record_type"] == 200
    pd_ = dss.read_paired_data(p, "/BASIN/LOC/STAGE-DAMAGE///PLAN A/")
    assert pd_.record_type == 200
    assert pd_.ordinates.dtype == np.float32 and pd_.values.dtype == np.float32
    np.testing.assert_array_equal(pd_.ordinates, x)
    np.testing.assert_array_equal(pd_.values, y.T)
    assert pd_.labels == ["Total", "Residential structures", "Commercial"]
    assert (pd_.units_independent, pd_.type_independent) == ("FT", "LINEAR")
    assert (pd_.units_dependent, pd_.type_dependent) == ("DOLLARS", "LINEAR")
    assert (pd_.x_precision, pd_.y_precision) == (1, 0)
    assert pd_.parameters == ("STAGE", "DAMAGE")
    df = pd_.to_dataframe()
    assert list(df.columns) == ["Total", "Residential structures", "Commercial"]


def test_float64_preserved_and_type_change(tmp_path, version):
    p = _new(tmp_path, version)
    path = "/A/B/FREQ-FLOW///F/"
    x = np.array([0.5, 0.1, 0.01])
    y = np.array([1.0 / 3.0, 2.0 / 3.0, 0.1 + 0.2])
    dss.write_paired_data(p, path, x, y, x_units="", x_type="PROB", y_units="CFS",
                          y_type="LOG", version=version)
    r = dss.read_paired_data(p, path)
    assert r.record_type == 205 and r.values.dtype == np.float64
    assert r.values[0].tobytes() == y.tobytes()
    # Rewrite as float32: the record becomes PD (200).
    rep = dss.write_paired_data(p, path, x.astype(np.float32), y.astype(np.float32),
                                x_units="", x_type="PROB", y_units="CFS", y_type="LOG",
                                version=version)
    assert rep["blocks"][0]["outcome"] == "replaced"
    r = dss.read_paired_data(p, path)
    assert r.record_type == 200
    # value_type forces the width.
    dss.write_paired_data(p, path, x, y, x_units="", x_type="PROB", y_units="CFS",
                          y_type="LOG", value_type="float", version=version)
    assert dss.read_paired_data(p, path).record_type == 200


def test_equal_rewrite_unchanged(tmp_path, version):
    p = _new(tmp_path, version)
    kw = dict(x_units="FT", x_type="LINEAR", y_units="CFS", y_type="LINEAR", version=version)
    dss.write_paired_data(p, "/A/B/STAGE-FLOW///F/", [1.0, 2.0], [10.0, 20.0], **kw)
    before = p.read_bytes()
    rep = dss.write_paired_data(p, "/A/B/STAGE-FLOW///F/", [1.0, 2.0], [10.0, 20.0], **kw)
    assert rep["blocks"][0]["outcome"] == "unchanged"
    assert p.read_bytes() == before


def test_dataframe_and_one_curve(tmp_path, version):
    p = _new(tmp_path, version)
    df = pd.DataFrame({"a": [1.0, 2.0, 3.0], "b": [4.0, 5.0, 6.0]})
    dss.write_paired_data(p, "/A/DF/STAGE-FLOW///F/", [0.0, 1.0, 2.0], df,
                          x_units="FT", x_type="LINEAR", y_units="CFS", y_type="LINEAR",
                          version=version)
    r = dss.read_paired_data(p, "/A/DF/STAGE-FLOW///F/")
    np.testing.assert_array_equal(r.values, df.to_numpy().T)
    assert r.labels == []  # column names are not labels


def test_nan_mapping_and_raw(tmp_path, version):
    p = _new(tmp_path, version)
    kw = dict(x_units="FT", x_type="LINEAR", y_units="CFS", y_type="LINEAR", version=version)
    dss.write_paired_data(p, "/A/NAN/STAGE-FLOW///F/", np.float32([1, 2]),
                          np.float32([np.nan, 3]), **kw)
    r = dss.read_paired_data(p, "/A/NAN/STAGE-FLOW///F/")
    want = np.float32(-901.0) if version == 6 else UNDEF32
    assert r.values[0, 0] == want
    dss.write_paired_data(p, "/A/RAW/STAGE-FLOW///F/", np.float32([1, 2]),
                          np.float32([np.nan, 3]), raw=True, **kw)
    assert np.isnan(dss.read_paired_data(p, "/A/RAW/STAGE-FLOW///F/").values[0, 0])
    with pytest.raises(ValueError, match="infinity"):
        dss.write_paired_data(p, "/A/INF/STAGE-FLOW///F/", [1.0, 2.0], [np.inf, 3.0], **kw)
    with pytest.raises(ValueError, match="overflows"):
        dss.write_paired_data(p, "/A/OVF/STAGE-FLOW///F/", [1.0, 2.0], [1e300, 3.0],
                              value_type="float", **kw)


def test_refusals_leave_file_unchanged(tmp_path, version):
    p = _new(tmp_path, version)
    kw = dict(x_units="FT", x_type="LINEAR", y_units="CFS", y_type="LINEAR", version=version)
    dss.write_paired_data(p, "/A/B/STAGE-FLOW///F/", [1.0, 2.0], [10.0, 20.0], **kw)
    dss.write_timeseries(p, "/A/B/FLOW//1HOUR/F/", np.float32([1, 2]),
                         start="01JAN2020 0100", units="CFS", data_type="INST-VAL",
                         version=version)
    before = p.read_bytes()
    bad = [
        dict(pathname="//R1/FIRST-QUARTILE/24HR///50%/"),  # 8 slashes
        dict(pathname="/A/B/C/D/E/"),
        dict(pathname="/A/B/STAGE\x01FLOW///F/"),
        dict(labels=["one", "two"]),
        dict(x_precision=2),
        dict(x_precision=12, y_precision=1),
        dict(y=[[1.0], [2.0], [3.0]]),
        dict(x=[]),
    ]
    for b in bad:
        args = dict(pathname="/A/C/STAGE-FLOW///F/", x=[1.0, 2.0], y=[10.0, 20.0])
        args.update(b)
        with pytest.raises(ValueError):
            dss.write_paired_data(p, args.pop("pathname"), args.pop("x"), args.pop("y"),
                                  **{**kw, **args})
        assert p.read_bytes() == before, b
    with pytest.raises(DssUnsupportedRecordError):
        dss.write_paired_data(p, "/A/B/FLOW/01JAN2020/1HOUR/F/", [1.0, 2.0], [1.0, 2.0], **kw)
    if version == 6:
        for b in (dict(y_units="123456789"), dict(time_zone="UTC"),
                  dict(x_precision=0, y_precision=2),
                  dict(independent_is_x_axis=False), dict(labels=["x "])):
            with pytest.raises(ValueError):
                dss.write_paired_data(p, "/A/C/STAGE-FLOW///F/", [1.0, 2.0], [10.0, 20.0],
                                      **{**kw, **b})
    assert p.read_bytes() == before


def test_b2_probability_pathnames_stay_distinct(tmp_path, version):
    """HMS's Atlas 14 import built an 8-slash pathname that heclib
    truncated, so every probability column of a quartile landed in one record."""
    p = _new(tmp_path, version)
    hours = np.arange(49, dtype=np.float64) * 0.5
    cum = "CUMULATI" if version == 6 else "CUMULATIVE"
    probs = ["10%", "50%", "90%"]
    for k, prob in enumerate(probs):
        frac = (np.arange(49) / 48.0) ** (1 + k)
        with pytest.raises(ValueError, match="7 slashes"):
            dss.write_paired_data(p, f"//SYN_R1/FIRST-QUARTILE/24HR///{prob}/", hours, frac,
                                  x_units="HOURS", x_type="TIME", y_units="FRACTION",
                                  y_type=cum, version=version)
        dss.write_paired_data(p, f"//SYN_R1/FIRST-QUARTILE/24HR//{prob}/", hours, frac,
                              x_units="HOURS", x_type="TIME", y_units="FRACTION",
                              y_type=cum, version=version)
    with dss.open(p) as f:
        cat = f.catalog()
        assert len(cat) == 3
    for k, prob in enumerate(probs):
        r = dss.read_paired_data(p, f"//SYN_R1/FIRST-QUARTILE/24HR//{prob}/")
        np.testing.assert_array_equal(r.values[0], (np.arange(49) / 48.0) ** (1 + k))
        assert r.type_independent == "TIME" and r.type_dependent == cum
        assert r.labels == []
    compat = dss.read_paired_data_compat(p, "//SYN_R1/FIRST-QUARTILE/24HR//50%/")
    assert compat["x_label"] == "TIME" and compat["x_units"] == "HOURS"


def test_dssfile_method_uses_file_version(tmp_path, version):
    p = _new(tmp_path, version)
    with dss.open(p) as f:
        rep = f.write_paired_data("/A/M/STAGE-FLOW///F/", [1.0, 2.0], [3.0, 4.0],
                                  x_units="FT", x_type="LINEAR", y_units="CFS",
                                  y_type="LINEAR")
        assert rep["version"] == version
        r = f.read_paired_data("/A/M/STAGE-FLOW///F/")
        np.testing.assert_array_equal(r.values[0], [3.0, 4.0])


def test_dss6_long_labels_and_upper_case(tmp_path):
    p = _new(tmp_path, 6)
    labels = ["A label longer than twelve", "short", " leading"]
    dss.write_paired_data(p, "/a/b/stage-flow///f/", [1.0, 2.0], np.ones((2, 3)),
                          x_units="ft", x_type="LINEAR", y_units="cfs", y_type="LINEAR",
                          labels=labels, version=6)
    r = dss.read_paired_data(p, "/A/B/STAGE-FLOW///F/")
    assert r.pathname == "/A/B/STAGE-FLOW///F/"
    assert r.labels == labels


def test_elev_unit_spec_refused(tmp_path, version):
    """heclib reads a '|' in an elevation-side unit as a unit specification
    with a vertical datum; the writer applies no datum and refuses it."""
    p = _new(tmp_path, version)
    before = p.read_bytes()
    with pytest.raises(DssUnsupportedRecordError):
        dss.write_paired_data(p, "/E/A/ELEV-STORAGE///F/", [100.0, 101.0], [1.0, 2.0],
                              x_units="FT|NGVD", x_type="LINEAR", y_units="AC-FT",
                              y_type="LINEAR", version=version)
    assert p.read_bytes() == before
    rep = dss.write_paired_data(p, "/E/A/ELEV-STORAGE///F/", [100.0, 101.0], [1.0, 2.0],
                                x_units="FT", x_type="LINEAR", y_units="AC-FT",
                                y_type="LINEAR", version=version)
    assert rep["blocks"][0]["outcome"] == "added"

"""Real HEC-written grids: engine values, metadata and CRS against HEC Monolith.

``fixtures/public/real_grid6.dss`` (DSS6) and ``real_grid7.dss`` (DSS7) were written by
HEC Monolith 3.3.27 through its Java API:

- five real records copied unchanged from public USACE samples: three SHG Albers
  PER-CUM precipitation grids from the HEC-RAS 7.0 example project
  ``BaldEagleCrkMulti2D`` (``Precipitation/precip.2018.09.dss``) and two HRAP PER-CUM
  grids from the HEC-HMS 4.13 sample ``tenk/hrap.dss``;
- two synthetic INST-VAL grids (temperature with one null cell, SWE) on a specified
  grid with a UTM zone 17N WKT, ZLIB compressed. No public sample has a specified or
  INST-VAL grid; their values are invented.

DSS6 keeps the PRECIP_2_BYTE encoding of the real records; DSS7 stores every record as
a ZLIB -100 grid. ``fixtures/public/real_grid_monolith.json`` holds what Monolith
returned for each record through ``HecDss.get`` (the path ras-commander's
``RasDss.read_grid`` uses). Monolith is not run here.
"""

import hashlib
import json
import struct

import numpy as np
import pandas as pd
import pytest

import ras_commander_fast_dss as dss
from ras_commander_fast_dss import GRID_UNDEFINED

from conftest import fixture_path

SHA256 = {
    "real_grid6.dss": "66dcdf3ed9c8474d13fdf95e78391bf2749a3bc0a3d5f87358008bb3cd44afb9",
    "real_grid7.dss": "48a6bef95a62cae1075eb95696df25ab1a4e60851634067106cd39733853c403",
}
REF = json.loads(fixture_path("real_grid_monolith.json").read_text(encoding="utf-8"))
RECORDS = [
    pytest.param(name, rec, id=f"{name}:{rec['pathname']}")
    for name, f in REF["files"].items()
    for rec in f["records"]
]
CLASS = {
    "albers": "hec.heclib.grid.AlbersInfo",
    "hrap": "hec.heclib.grid.HrapInfo",
    "specified": "hec.heclib.grid.SpecifiedGridInfo",
}
MONTHS = (
    "January", "February", "March", "April", "May", "June",
    "July", "August", "September", "October", "November", "December",
)


def bits(v) -> str:
    return format(struct.unpack("<I", struct.pack("<f", float(v)))[0], "08x")


def hec_time(t) -> str:
    """HecTime's ``getStartTime``/``getEndTime`` text, e.g. ``1 January 2020, 06:00``."""
    t = pd.Timestamp(t)
    return f"{t.day} {MONTHS[t.month - 1]} {t.year}, {t.hour:02d}:{t.minute:02d}"


def sha(p) -> str:
    return hashlib.sha256(p.read_bytes()).hexdigest()


def dss_parts_time(part: str):
    """RasDss._parse_grid_dss_datetime for D/E parts (``01SEP2018:0100``; 2400 rolls)."""
    if not part:
        return None
    day, clock = part.split(":")
    t = pd.Timestamp(pd.to_datetime(day, format="%d%b%Y"))
    if clock == "2400":
        return t + pd.Timedelta(days=1)
    return t + pd.Timedelta(hours=int(clock[:2]), minutes=int(clock[2:]))


def ras_commander_grid_dict(path, pathname):
    """``RasDss.read_grid``'s dict, built from the engine's Grid instead of Java.

    Mirrors ras-commander ``ras_commander/dss/RasDss.py`` ``read_grid`` key for key:
    ``data`` is float32 ``(rows, columns)`` with row 0 south, and only the HEC grid
    no-data value (-FLT_MAX) becomes NaN.
    """
    g = dss.read_grid(path, pathname, missing="raw")
    values = g.data.copy()
    values[values == np.float32(GRID_UNDEFINED)] = np.nan
    parts = pathname.split("/")[1:7]
    projection = {}
    origin = None
    if g.grid_type in ("albers", "specified"):
        projection.update(
            {
                "x_coord_cell_zero": float(np.float32(g.x_coord_of_grid_cell_zero)),
                "y_coord_cell_zero": float(np.float32(g.y_coord_of_grid_cell_zero)),
            }
        )
        origin = (
            projection["x_coord_cell_zero"] + g.lower_left_cell_x * g.cell_size,
            projection["y_coord_cell_zero"] + g.lower_left_cell_y * g.cell_size,
        )
    a = g.metadata.get("albers")
    if g.grid_type == "albers" and a is not None:
        projection.update(
            {
                "datum_code": int(a["datum"]),
                "units": str(a["units"]),
                "standard_parallel_1": float(a["first_standard_parallel"]),
                "standard_parallel_2": float(a["second_standard_parallel"]),
                "central_meridian": float(a["central_meridian"]),
                "latitude_of_origin": float(a["latitude_of_projection_origin"]),
                "false_easting": float(a["false_easting"]),
                "false_northing": float(a["false_northing"]),
            }
        )
    start = hec_time(g.start_time) if g.start_time is not None else None
    end = hec_time(g.end_time) if g.end_time is not None else None
    m = g.metadata
    return {
        "dss_file": str(path),
        "pathname": pathname,
        "data": values,
        "shape": (g.ny, g.nx),
        "units": g.data_units,
        "data_type": g.data_type_name,
        "grid_type": g.grid_type,
        "crs": g.srs_definition,
        "cell_size": float(g.cell_size),
        "start_time": dss_parts_time(parts[3]),
        "end_time": dss_parts_time(parts[4]),
        "metadata": {
            "pathname_parts": dict(zip("ABCDEF", parts)),
            "grid_class": CLASS[g.grid_type],
            "grid_type_code": g.record_type,
            "data_type_code": g.data_type,
            "shape": (g.ny, g.nx),
            "number_of_cells_x": g.nx,
            "number_of_cells_y": g.ny,
            "lower_left_cell": (g.lower_left_cell_x, g.lower_left_cell_y),
            "origin": origin,
            "projection": projection,
            "number_missing": int(np.count_nonzero((g.data == np.float32(GRID_UNDEFINED)) | (g.data == -9999.0))),
            "nodata_value": float(np.float32(GRID_UNDEFINED)),
            "compression": {
                "method": int(m["compression_method"]),
                "base": float(m["compression_base"]),
                "scale_factor": float(m["compression_scale_factor"]),
                "element_size": int(m["sizeof_compressed_elements"]),
            },
            "timing": {"start": start, "end": end, "period": f"{start} to {end}"},
        },
    }


REF_SHA256 = "7ccd74cca490a961a283e86ae1164f96936f3a3fea800d6dd257b2451d59ed1f"


def test_reference_json_sha256():
    assert sha(fixture_path("real_grid_monolith.json")) == REF_SHA256


@pytest.mark.parametrize("name", sorted(SHA256))
def test_fixture_sha256_and_read_only(name):
    p = fixture_path(name)
    assert sha(p) == SHA256[name]
    with dss.open(p) as f:
        grids = f.catalog()
        assert list(grids.record_type // 10) == [42, 42, 42, 41, 41, 43, 43]
        for pathname in grids.pathname:
            f.read_grid(pathname)
    assert sha(p) == SHA256[name], "reader modified the fixture"


@pytest.mark.parametrize("name", sorted(SHA256))
def test_catalog_matches_monolith(name):
    with dss.open(fixture_path(name)) as f:
        assert f.version == REF["files"][name]["dss_version"]
        assert sorted(f.catalog().pathname) == sorted(r["pathname"] for r in REF["files"][name]["records"])


@pytest.mark.parametrize("name,rec", RECORDS)
def test_values_metadata_and_crs_match_monolith(name, rec):
    g = dss.read_grid(fixture_path(name), rec["pathname"], missing="raw")
    m = g.metadata
    assert CLASS[g.grid_type] == rec["grid_class"]
    assert g.record_type == rec["grid_type_code"]
    assert (g.data_type, g.data_type_name) == (rec["data_type_code"], rec["data_type"])
    assert g.data_units == rec["units"]
    assert (g.nx, g.ny) == (rec["nx"], rec["ny"])
    assert [g.lower_left_cell_x, g.lower_left_cell_y] == rec["lower_left_cell"]
    assert bits(g.cell_size) == rec["cell_size_bits"]
    assert g.srs_definition == rec["crs"]
    if "cell_zero_bits" in rec:
        assert [bits(g.x_coord_of_grid_cell_zero), bits(g.y_coord_of_grid_cell_zero)] == rec["cell_zero_bits"]
    assert {
        "method": m["compression_method"],
        "base_bits": bits(m["compression_base"]),
        "scale_factor_bits": bits(m["compression_scale_factor"]),
        "element_size": m["sizeof_compressed_elements"],
    } == rec["compression"]
    assert [bits(g.min_value), bits(g.max_value), bits(g.mean_value)] == [
        rec["min_bits"], rec["max_bits"], rec["mean_bits"]
    ]
    start, end = hec_time(g.start_time), hec_time(g.end_time)
    assert {"start": start, "end": end, "period": f"{start} to {end}"} == rec["timing"]
    # Cells: native order is Monolith's GridData.getData() order (row 0 south).
    assert g.data.dtype == np.float32 and g.data.size == rec["n_cells"]
    assert hashlib.sha256(np.ascontiguousarray(g.data, "<f4").tobytes()).hexdigest() == rec["data_sha256"]
    flat = g.data.ravel()
    assert [[i, bits(flat[i])] for i, _ in rec["samples"]] == rec["samples"]
    assert int(np.count_nonzero(flat == np.float32(GRID_UNDEFINED))) == rec["n_nodata"]
    assert int(np.count_nonzero(np.isnan(flat))) == rec["n_nan"]
    # north_up is the same cells with the rows flipped.
    up = dss.read_grid(fixture_path(name), rec["pathname"], missing="raw", order="north_up")
    assert np.array_equal(up.data[::-1].view(np.uint32), g.data.view(np.uint32))


def _albers_params(rec):
    a = rec["albers"]
    return [a["datum_code"], a["units"], a["standard_parallel_1"], a["standard_parallel_2"],
            a["central_meridian"], a["latitude_of_origin"], a["false_easting"], a["false_northing"]]


ALBERS = [p for p in RECORDS if "albers" in p.values[1]]


# Modern (-100) Albers parameters are parsed from the stored WKT, as Monolith's
# GridStructContainer.setAlbersSpatialReferenceInfo does; legacy ones are as stored.
@pytest.mark.parametrize("name,rec", ALBERS)
def test_albers_parameters_match_monolith(name, rec):
    a = dss.read_grid(fixture_path(name), rec["pathname"]).metadata["albers"]
    assert a is not None
    got = [a["datum"], a["units"], bits(a["first_standard_parallel"]), bits(a["second_standard_parallel"]),
           bits(a["central_meridian"]), bits(a["latitude_of_projection_origin"]),
           bits(a["false_easting"]), bits(a["false_northing"])]
    assert got == _albers_params(rec)


def _null_zero(p):
    # Monolith writes -100 HRAP/Albers grids with null value 0.0; the records
    # holding 0.0 cells (stored minimum 0.0) are the ones the old rule got wrong.
    rec = p.values[1]
    return (
        p.values[0] == "real_grid7.dss"
        and rec["grid_class"] != CLASS["specified"]
        and rec["min_bits"] == "00000000"
    )


# null_value is a missing marker for specified grids only, as in Monolith.
@pytest.mark.parametrize("name,rec", RECORDS)
def test_missing_cells_match_monolith(name, rec):
    g = dss.read_grid(fixture_path(name), rec["pathname"])  # missing="nan"
    assert int(np.count_nonzero(g.missing_mask())) == rec["number_missing"]
    assert int(np.count_nonzero(np.isnan(g.data))) == rec["number_missing"]


def test_affected_records_store_null_zero_and_keep_zero_cells():
    affected = [p for p in RECORDS if _null_zero(p)]
    assert len(affected) == 4
    for p in affected:
        name, rec = p.values
        g = dss.read_grid(fixture_path(name), rec["pathname"])
        assert g.null_value == 0.0 and g.grid_type in ("hrap", "albers")
        raw = dss.read_grid(fixture_path(name), rec["pathname"], missing="raw")
        zeros = raw.data == 0.0
        assert zeros.any() and not np.isnan(g.data[zeros]).any()


def test_grid_missing_mask_null_value_rule():
    from ras_commander_fast_dss._records import grid_missing_mask

    v = np.array([0.0, -9999.0, np.nan, -901.0, 5.0], dtype=np.float32)
    assert grid_missing_mask(v, 0.0, "albers").tolist() == [False, False, True, True, False]
    assert grid_missing_mask(v, 0.0, "hrap").tolist() == [False, False, True, True, False]
    assert grid_missing_mask(v, -9999.0, "specified").tolist() == [False, True, True, True, False]
    # grid_type unknown: the null value applies (the earlier rule).
    assert grid_missing_mask(v, 0.0).tolist() == [True, False, True, True, False]


@pytest.mark.parametrize("name,rec", RECORDS)
def test_ras_commander_grid_dict_shape(name, rec):
    rc = REF["ras_commander_read_grid"]
    p = fixture_path(name)
    d = ras_commander_grid_dict(p, rec["pathname"])
    assert list(d) == rc["keys"]
    assert list(d["metadata"]) == rc["metadata_keys"]
    assert d["data"].dtype == np.float32 and d["data"].shape == d["shape"] == (rec["ny"], rec["nx"])
    assert d["grid_type"] == rc["grid_type_names"][rec["grid_class"].rsplit(".", 1)[1]]
    assert (d["units"], d["data_type"], d["crs"]) == (rec["units"], rec["data_type"], rec["crs"])
    assert bits(d["cell_size"]) == rec["cell_size_bits"]
    md = d["metadata"]
    assert md["grid_class"] == rec["grid_class"]
    assert (md["grid_type_code"], md["data_type_code"]) == (rec["grid_type_code"], rec["data_type_code"])
    assert list(md["lower_left_cell"]) == rec["lower_left_cell"]
    assert md["number_missing"] == rec["number_missing"]
    assert md["nodata_value"] == float(np.float32(GRID_UNDEFINED))
    assert md["timing"] == rec["timing"]
    c = md["compression"]
    assert [c["method"], bits(c["base"]), bits(c["scale_factor"]), c["element_size"]] == [
        rec["compression"]["method"], rec["compression"]["base_bits"],
        rec["compression"]["scale_factor_bits"], rec["compression"]["element_size"],
    ]
    if "cell_zero_bits" in rec:
        pr = md["projection"]
        assert [bits(pr["x_coord_cell_zero"]), bits(pr["y_coord_cell_zero"])] == rec["cell_zero_bits"]
        assert md["origin"] is not None
    else:
        assert md["projection"] == {} and md["origin"] is None
    # Time window from the D/E parts; INST-VAL grids have an empty E part.
    assert d["start_time"] == pd.Timestamp(np.datetime64(dss.read_grid(p, rec["pathname"]).start_time))
    if rec["data_type"] == "INST-VAL":
        assert d["end_time"] is None
    # data: only -FLT_MAX is NaN, everything else keeps Monolith's float32 bits.
    nan = np.isnan(d["data"]).ravel()
    assert int(nan.sum()) == rec["n_nodata"]
    raw = np.where(nan, np.float32(GRID_UNDEFINED), d["data"].ravel()).astype("<f4")
    assert hashlib.sha256(raw.tobytes()).hexdigest() == rec["data_sha256"]

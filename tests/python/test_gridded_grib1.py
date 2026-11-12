"""P73: Stage IV GRIB1 through the public Python API, on a synthetic message built here."""
import struct

import numpy as np
import pytest

import ras_commander_fast_dss as dss
from ras_commander_fast_dss import gridded

VARIABLE = "Total_precipitation_surface_1_Hour_Accumulation"


def _u24(n):
    return bytes([(n >> 16) & 255, (n >> 8) & 255, n & 255])


def _sm3(v):
    b = bytearray(_u24(abs(v)))
    if v < 0:
        b[0] |= 0x80
    return bytes(b)


def stage4_message(nx=40, ny=30, bds_flag=0):
    """One Stage IV-shaped GRIB1 message: HRAP corner, 4763 m, 1 h APCP ending 2008-01-08 12Z."""
    pds = bytearray(28)
    pds[0:3] = _u24(28)
    pds[3], pds[4], pds[5], pds[6], pds[7] = 2, 7, 182, 255, 0x80
    pds[8], pds[9] = 61, 1
    pds[12:16] = bytes([8, 1, 8, 11])
    pds[17], pds[18], pds[19], pds[20] = 1, 0, 1, 4
    pds[24], pds[25] = 21, 4
    pds[26:28] = bytes([0, 1])  # D = 1
    gds = bytearray(32)
    gds[0:3] = _u24(32)
    gds[4], gds[5] = 255, 5
    gds[6:8] = struct.pack(">H", nx)
    gds[8:10] = struct.pack(">H", ny)
    gds[10:13] = _sm3(23117)
    gds[13:16] = _sm3(-119023)
    gds[16] = 0x08
    gds[17:20] = _sm3(-105000)
    gds[20:23] = _u24(4763)
    gds[23:26] = _u24(4763)
    gds[27] = 64
    n = nx * ny
    values = (np.arange(n) % 50).astype(np.uint64)
    bits = 8
    payload = values.astype(np.uint8).tobytes()
    bds = _u24(11 + len(payload)) + bytes([bds_flag]) + bytes([0, 0]) + bytes([0, 0, 0, 0]) + bytes([bits]) + payload
    body = b"GRIB" + b"\0\0\0\x01" + bytes(pds) + bytes(gds) + bds + b"7777"
    body = bytearray(body)
    body[4:7] = _u24(len(body))
    return bytes(body)


def test_features_advertise_grib1_stage4():
    assert "gridded_grib1_stage4" in dss.FEATURES


def test_list_variables_and_import(tmp_path):
    src = tmp_path / "ST4.2008010812.01h"
    src.write_bytes(stage4_message())
    assert gridded.list_variables(src) == [VARIABLE]
    out = tmp_path / "out.dss"
    # A synthetic corner grid has no whole-cell SHG origin; Vortex's own extent rule takes it.
    result = gridded.import_gridded(src, out, variable=VARIABLE, align="vortex",
                                    parts={"A": "SHG", "B": "P73", "F": "TEST"})
    assert result.pathnames == ["/SHG/P73/PRECIPITATION/08JAN2008:1100/08JAN2008:1200/TEST/"]
    with dss.open(out) as f:
        assert sorted(str(p) for p in f.catalog()["pathname"].tolist()) == result.pathnames
        grid = f.read_grid(result.pathnames[0])
        assert np.isfinite(np.asarray(grid.data, dtype=np.float64)).any()


def test_additional_flags_are_refused(tmp_path):
    src = tmp_path / "ST4.2008010812.01h"
    src.write_bytes(stage4_message(bds_flag=0x10))
    with pytest.raises(Exception, match="additional flags"):
        gridded.import_gridded(src, tmp_path / "out.dss", variable=VARIABLE, align="vortex")
    assert not (tmp_path / "out.dss").exists()

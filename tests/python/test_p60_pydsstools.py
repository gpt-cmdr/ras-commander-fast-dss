"""P60: engine findings E1-E3 of the P58 pydsstools validation.

``p60_pydsstools6.dss`` is written by pydsstools 3.1.0 from synthetic values
(``evidence/P60/scripts/pyd_fixture.py``). It holds a regular series
whose units word is ``CFS``, NUL and stray bytes (E1), an irregular series, and a
specified grid whose header byte count exceeds the stored header (E2). E3: DSS6
time-invariant grids are written as 410/420/430, as HEC's DSS6 grid store does; DSS7
HRAP and Albers ones as 410/420 (Monolith cannot read 411/421), DSS7 specified ones 431.
"""
import hashlib

import numpy as np

import ras_commander_fast_dss as dss
from conftest import fixture_path

SHA256 = "467fe50e0281ba90cf2837c51439e62b1e83399c539b3394e05c67eb0162c19b"
RTS = "/P60/RTS/FLOW/01JAN2021/1HOUR/PYD/"
ITS = "/P60/ITS/STAGE/01JAN2021/IR-MONTH/PYD/"
GRID = "/UTM17N/P60/PRECIP/01JUN2021:0100/01JUN2021:0200/PYD/"


def test_fixture_sha256():
    assert hashlib.sha256(fixture_path("p60_pydsstools6.dss").read_bytes()).hexdigest() == SHA256


def test_e1_units_cut_at_first_nul():
    with dss.open(fixture_path("p60_pydsstools6.dss")) as f:
        r = f.read_record_arrays(RTS)
        i = f.read_record_arrays(ITS)
    assert (r["units"], r["data_type"]) == ("CFS", "INST-VAL")
    assert (i["units"], i["data_type"]) == ("FEET", "INST-VAL")


def test_e2_grid_header_count_beyond_stored_header():
    with dss.open(fixture_path("p60_pydsstools6.dss")) as f:
        g = f.read_grid(GRID)
    assert g.record_type == 430 and g.data.shape == (2, 3)
    assert g.null_value == -9999.0 and np.isnan(g.data[0, 1])
    assert g.data[1].tolist() == [1.0, 2.0, 3.0]


def test_e3_dss6_time_invariant_types(tmp_path):
    for v in (6, 7):
        p = tmp_path / f"t{v}.dss"
        dss.create(p, version=v)
        dss.write_grid(p, "/SHG/P60/ELEVATION///T/", np.arange(6, dtype=np.float32).reshape(2, 3),
                       time_stamped=False, grid_type="shg", cell_size=2000.0, lower_left_cell=(0, 0),
                       units="M", data_type="INST-VAL", version=v)
        with dss.open(p) as f:
            g = f.read_grid("/SHG/P60/ELEVATION///T/")
        assert g.record_type == 420 and g.start_time is None

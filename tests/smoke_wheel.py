"""Installed-wheel smoke test: run with `python -I tests/smoke_wheel.py` after installing a wheel.

Uses only the installed package and two public fixtures, so it runs on every wheel platform.
"""
import importlib.metadata
import os
import re
import sys
import tempfile
from pathlib import Path

import numpy as np

import ras_commander_fast_dss as dss

root = Path(__file__).resolve().parents[1]
assert not Path(dss.__file__).resolve().is_relative_to(root / "python"), "imported the source tree, not the wheel"
dist = importlib.metadata.distribution("ras-commander-fast-dss")
version = dist.version
# The imported module must belong to the installed distribution whose version is checked.
owned = Path(dist.locate_file("ras_commander_fast_dss/__init__.py")).resolve()
assert owned == Path(dss.__file__).resolve(), f"{dss.__file__} is not from the installed distribution ({owned})"
# The installed version must be the one this checkout builds, and the tag's on a tag build.
cargo = (root / "Cargo.toml").read_text(encoding="utf-8")
workspace = re.search(r'^\[workspace\.package\][^\[]*?^version\s*=\s*"([^"]+)"', cargo, re.M | re.S).group(1)
assert version == workspace, f"installed {version}, but Cargo.toml says {workspace}"
if os.environ.get("GITHUB_REF_TYPE") == "tag":
    tag = os.environ["GITHUB_REF_NAME"]
    assert tag == f"v{version}", f"installed {version}, but the tag is {tag}"
print(f"ras-commander-fast-dss {version} from {Path(dss.__file__).parent}")

fixtures = root / "fixtures" / "public"
with dss.open(fixtures / "sample7.dss") as f:
    assert (f.version, f.version_string) == (7, "7-10")
    assert len(f.catalog()) == 594
assert len(dss.get_catalog(fixtures / "castro.dss")) > 0

with tempfile.TemporaryDirectory() as tmp:
    for file_version in (7, 6):
        out = Path(tmp) / f"smoke{file_version}.dss"
        dss.create(out, version=file_version)
        values = np.array([10.0, 12.5, 15.0])
        dss.write_timeseries(out, "/SMOKE/LOC/FLOW/01JAN2020/1HOUR/SIM/", values, start="2020-01-01 01:00",
                             units="CFS", data_type="INST-VAL", version=file_version)
        df = dss.read_timeseries(out, "/SMOKE/LOC/FLOW//1HOUR/SIM/")
        assert np.array_equal(df["value"].to_numpy(), values), df
assert "gridded_import" in dss.FEATURES
print(f"smoke passed on Python {sys.version.split()[0]}")

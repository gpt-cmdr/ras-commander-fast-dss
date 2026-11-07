"""The wheel carries the licences of the crates linked into it."""
from importlib import resources


def test_installed_package_ships_the_licence_bundle():
    bundle = resources.files("ras_commander_fast_dss") / "THIRD_PARTY_LICENSES.txt"
    text = bundle.read_text(encoding="utf-8")
    for crate in ("pyo3", "numpy", "hayro-jpeg2000", "netcdf-reader", "libm", "flate2", "weezl"):
        assert f"\ncrate {crate} " in text
    assert "Permission is hereby granted" in text
    assert "Apache License" in text

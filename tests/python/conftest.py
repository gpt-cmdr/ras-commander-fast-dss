"""Shared fixtures for the ras_commander_fast_dss Python tests (public fixtures only)."""

from pathlib import Path

import pytest

FIXTURES = Path(__file__).resolve().parents[2] / "fixtures" / "public"

DSS7 = ["sample7.dss", "BaldEagleDamBrk.dss"]
DSS6 = ["sample6.dss", "castro.dss", "green_river.dss", "Muncie.dss", "sample_vb.dss", "synth6.dss"]
ALL = DSS7 + DSS6


def pytest_configure(config):
    # Apply for both explicit tests/python runs and the root pyproject testpaths,
    # without changing configuration outside this task's permitted test scope.
    config.addinivalue_line("filterwarnings", "error::pytest.PytestUnhandledThreadExceptionWarning")


def fixture_path(name: str) -> Path:
    p = FIXTURES / name
    if not p.exists():
        pytest.fail(f"public fixture missing: {p}")
    return p


@pytest.fixture(params=ALL)
def any_fixture(request) -> Path:
    return fixture_path(request.param)

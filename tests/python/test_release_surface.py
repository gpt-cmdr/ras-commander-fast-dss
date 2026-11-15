"""A release wheel has no fault-injection entry point.

Fault injection for the publication of new files (``_set_publish_faults``) is compiled only
with the ``test-hooks`` Cargo feature. ``_native._TEST_HOOKS`` reports how the extension was
built. CI runs this file against the release wheel with ``FAST_DSS_RELEASE_WHEEL=1``, then
runs the whole suite against a separate test wheel built with ``--features test-hooks``.
"""

from __future__ import annotations

import os

import pytest

import ras_commander_fast_dss as dss

FAULT_ENTRY_POINTS = ("_set_publish_faults",)


def test_hooks_match_the_build_flag():
    present = [n for n in FAULT_ENTRY_POINTS if hasattr(dss._native, n)]
    assert isinstance(dss._native._TEST_HOOKS, bool)
    assert bool(present) == dss._native._TEST_HOOKS, present


@pytest.mark.skipif(os.environ.get("FAST_DSS_RELEASE_WHEEL") != "1",
                    reason="set FAST_DSS_RELEASE_WHEEL=1 to check a release wheel")
def test_release_wheel_has_no_fault_injection():
    assert dss._native._TEST_HOOKS is False
    names = [n for n in dir(dss._native) if "fault" in n.lower()]
    assert names == [], names
    for name in FAULT_ENTRY_POINTS:
        assert not hasattr(dss._native, name), name

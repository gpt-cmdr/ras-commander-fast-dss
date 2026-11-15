"""The release workflow's asset attachment and PyPI check (``scripts/release_assets.py``).

The publish job must be safe to rerun: after a first attachment, after a partial one, and when
PyPI was skipped or interrupted, a rerun keeps byte-identical assets, uploads what is missing
and reaches PyPI. It must never attach or keep artifacts from another build. A fake release
stands in for ``gh``; no network is used.
"""
from __future__ import annotations

import hashlib
import importlib.util
import shutil
from pathlib import Path

import pytest

SCRIPT = Path(__file__).resolve().parents[2] / "scripts" / "release_assets.py"
if not SCRIPT.is_file():
    pytest.skip("scripts/release_assets.py is not in this checkout", allow_module_level=True)
spec = importlib.util.spec_from_file_location("release_assets", SCRIPT)
ra = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ra)


class FakeRelease:
    """Assets of one release in a folder; ``upload`` refuses an existing name, as gh does."""

    def __init__(self, root: Path):
        self.root = root
        root.mkdir()
        self.states: dict[str, str] = {}
        self.uploads: list[list[str]] = []

    def assets(self):
        return [{"name": p.name, "state": self.states.get(p.name, "uploaded")}
                for p in sorted(self.root.iterdir())]

    def download(self, name, dest):
        shutil.copyfile(self.root / name, dest / name)
        return dest / name

    def upload(self, paths):
        names = [p.name for p in paths]
        for p in paths:
            if (self.root / p.name).exists():
                raise RuntimeError(f"asset {p.name} already exists")
        for p in paths:
            shutil.copyfile(p, self.root / p.name)
        self.uploads.append(names)


def build(tmp: Path, tag: str = "a") -> tuple[list[Path], Path]:
    dist = tmp / f"dist-{tag}"
    dist.mkdir()
    for name in ("pkg-0.1.3-cp310-abi3-win_amd64.whl", "pkg-0.1.3.tar.gz"):
        (dist / name).write_bytes(f"{name} built by {tag}".encode())
    sums = tmp / f"SHA256SUMS-{tag}"
    sums.write_text("".join(f"{hashlib.sha256(p.read_bytes()).hexdigest()}  {p.name}\n"
                            for p in sorted(dist.iterdir())), encoding="utf-8")
    named = tmp / f"sums-{tag}" / "SHA256SUMS"
    named.parent.mkdir()
    shutil.copyfile(sums, named)
    return sorted(dist.iterdir()) + [named], named


def test_first_attachment_uploads_everything(tmp_path):
    rel = FakeRelease(tmp_path / "release")
    files, sums = build(tmp_path)
    ra.attach(rel, files, sums)
    assert [sorted(u) for u in rel.uploads] == [sorted(p.name for p in files)]
    assert sorted(a["name"] for a in rel.assets()) == sorted(p.name for p in files)


def test_rerun_after_complete_attachment_keeps_identical_assets(tmp_path):
    rel = FakeRelease(tmp_path / "release")
    files, sums = build(tmp_path)
    ra.attach(rel, files, sums)
    ra.attach(rel, files, sums)  # the rerun must not fail and must not upload again
    assert len(rel.uploads) == 1


def test_rerun_after_partial_attachment_uploads_only_the_rest(tmp_path):
    rel = FakeRelease(tmp_path / "release")
    files, sums = build(tmp_path)
    shutil.copyfile(files[0], rel.root / files[0].name)
    ra.attach(rel, files, sums)
    assert [sorted(u) for u in rel.uploads] == [sorted(p.name for p in files[1:])]


def test_an_asset_with_other_bytes_is_refused_and_nothing_uploaded(tmp_path):
    rel = FakeRelease(tmp_path / "release")
    first, _ = build(tmp_path, "a")
    shutil.copyfile(first[0], rel.root / first[0].name)  # a wheel from build a is attached
    second, sums_second = build(tmp_path, "b")  # build b has the same names, other bytes
    before = {p.name: p.read_bytes() for p in rel.root.iterdir()}
    with pytest.raises(SystemExit, match="other bytes"):
        ra.attach(rel, second, sums_second)
    assert {p.name: p.read_bytes() for p in rel.root.iterdir()} == before
    assert rel.uploads == []


def test_an_incomplete_or_foreign_asset_is_refused(tmp_path):
    rel = FakeRelease(tmp_path / "release")
    files, sums = build(tmp_path)
    shutil.copyfile(files[0], rel.root / files[0].name)
    rel.states[files[0].name] = "starter"
    with pytest.raises(SystemExit, match="incomplete upload"):
        ra.attach(rel, files, sums)
    rel.states.clear()
    (rel.root / "notes.txt").write_bytes(b"attached by hand")
    with pytest.raises(SystemExit, match="not produced by this run"):
        ra.attach(rel, files, sums)
    assert rel.uploads == []


def test_pypi_check_lets_a_rerun_reach_pypi_only_with_identical_files():
    local = {"a.whl": "1" * 64, "a.tar.gz": "2" * 64}
    # Nothing on PyPI yet (PUBLISH_PYPI enabled for a rerun): the upload may proceed.
    assert ra.check_pypi(local, None, require_all=False) == []
    # An interrupted upload left one identical file: skip-existing may finish the rest.
    assert ra.check_pypi(local, {"a.whl": "1" * 64}, require_all=False) == []
    # ...but the final check still requires every file.
    assert ra.check_pypi(local, {"a.whl": "1" * 64}, require_all=True) == ["a.tar.gz: not on PyPI"]
    assert ra.check_pypi(local, dict(local), require_all=True) == []
    # A file of the same name with other bytes, or a file this run did not build, fails.
    assert "other bytes" in ra.check_pypi(local, {"a.whl": "9" * 64}, require_all=False)[0]
    assert "not built by this run" in ra.check_pypi(local, {"b.whl": "3" * 64}, require_all=False)[0]

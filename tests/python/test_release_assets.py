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
import sys
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


# -- pypi-check through main(): the distributions are exactly those in SHA256SUMS -------------
# main() reads pyproject.toml with tomllib (Python 3.11+); the release job runs Python 3.13.
needs_tomllib = pytest.mark.skipif(sys.version_info < (3, 11), reason="pypi-check needs tomllib")


def dist_with_sums(tmp: Path) -> tuple[Path, Path, dict[str, str]]:
    files, sums = build(tmp)
    dist = files[0].parent
    return dist, sums, {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in files[:-1]}


def run_pypi_check(monkeypatch, dist, sums, remote, *flags):
    """``main(["pypi-check", ...])`` against a fixed PyPI inventory; no network, no sleeping.
    Returns the number of PyPI queries."""
    calls = []

    def fake_pypi_files(name, version):
        calls.append((name, version))
        return remote

    monkeypatch.setattr(ra, "pypi_files", fake_pypi_files)
    monkeypatch.setattr(ra.time, "sleep", lambda s: None)
    ra.main(["pypi-check", str(dist), str(sums), *flags])
    return len(calls)


def add_attestations(dist: Path) -> None:
    """What pypa/gh-action-pypi-publish leaves after a Trusted Publishing upload."""
    for p in sorted(dist.iterdir()):
        (dist / f"{p.name}.publish.attestation").write_text('{"version": 1}', encoding="utf-8")


@needs_tomllib
def test_final_pypi_check_passes_with_attestation_sidecars(tmp_path, monkeypatch, capsys):
    dist, sums, local = dist_with_sums(tmp_path)
    add_attestations(dist)
    assert len(list(dist.iterdir())) == 4
    assert run_pypi_check(monkeypatch, dist, sums, dict(local), "--require-all") == 1
    assert "all present" in capsys.readouterr().out


@needs_tomllib
def test_final_pypi_check_with_sidecars_still_requires_every_distribution(tmp_path, monkeypatch):
    dist, sums, local = dist_with_sums(tmp_path)
    add_attestations(dist)
    whl = next(n for n in local if n.endswith(".whl"))
    sdist = next(n for n in local if n.endswith(".tar.gz"))
    # Missing on PyPI: retried, then refused. The sidecars are never reported.
    with pytest.raises(SystemExit) as e:
        run_pypi_check(monkeypatch, dist, sums, {whl: local[whl]}, "--require-all")
    assert f"{sdist}: not on PyPI" in str(e.value)
    assert "attestation" not in str(e.value)
    # Other bytes on PyPI: refused.
    with pytest.raises(SystemExit, match="other bytes"):
        run_pypi_check(monkeypatch, dist, sums, {**local, whl: "9" * 64}, "--require-all")
    # A sidecar is never accepted as a stand-in for its distribution.
    (dist / sdist).unlink()
    with pytest.raises(SystemExit, match=f"{sdist}: listed in SHA256SUMS but not in"):
        run_pypi_check(monkeypatch, dist, sums, dict(local), "--require-all")


@needs_tomllib
def test_pre_upload_pypi_check_refuses_unlisted_or_changed_files_before_pypi(tmp_path, monkeypatch):
    """The check before the upload fails before contacting PyPI if dist/ holds anything the
    action would upload that is not a distribution in SHA256SUMS, or a listed distribution
    whose bytes changed."""
    dist, sums, local = dist_with_sums(tmp_path)
    assert run_pypi_check(monkeypatch, dist, sums, None) == 1  # nothing on PyPI yet: proceed
    stray = dist / "pkg-0.1.3-cp310-abi3-linux_x86_64.whl"
    stray.write_bytes(b"not in SHA256SUMS")
    with pytest.raises(SystemExit, match="not a distribution listed in SHA256SUMS"):
        run_pypi_check(monkeypatch, dist, sums, None)
    stray.unlink()
    # An attestation for a file that is not listed is not a known sidecar.
    (dist / "other-0.1.3.tar.gz.publish.attestation").write_text("{}", encoding="utf-8")
    with pytest.raises(SystemExit, match="other-0.1.3.tar.gz.publish.attestation"):
        run_pypi_check(monkeypatch, dist, sums, None)
    (dist / "other-0.1.3.tar.gz.publish.attestation").unlink()
    whl = next(n for n in local if n.endswith(".whl"))
    (dist / whl).write_bytes(b"rebuilt")
    calls = []
    monkeypatch.setattr(ra, "pypi_files", lambda *a: calls.append(a))
    with pytest.raises(SystemExit, match=f"{whl}: local bytes .* differ from SHA256SUMS"):
        ra.main(["pypi-check", str(dist), str(sums)])
    assert calls == []


@needs_tomllib
def test_pypi_check_refuses_a_manifest_that_lists_no_distribution(tmp_path, monkeypatch):
    dist, sums, _ = dist_with_sums(tmp_path)
    (dist / "notes.txt").write_text("x", encoding="utf-8")
    sums.write_text(sums.read_text(encoding="utf-8")
                    + f"{hashlib.sha256(b'x').hexdigest()}  notes.txt\n", encoding="utf-8")
    with pytest.raises(SystemExit, match="notes.txt: listed in SHA256SUMS but not a wheel or sdist"):
        run_pypi_check(monkeypatch, dist, sums, None)
    sums.write_text("", encoding="utf-8")
    with pytest.raises(SystemExit, match="lists no distributions"):
        run_pypi_check(monkeypatch, dist, sums, None)
    # The old two-argument form is refused rather than run without the manifest.
    with pytest.raises(SystemExit):
        ra.main(["pypi-check", str(dist), "--require-all"])

"""Retry-safe attachment of release artifacts, and the PyPI identity check (release.yml).

    python scripts/release_assets.py attach <tag> <dist dir> <SHA256SUMS>
    python scripts/release_assets.py pypi-check <dist dir> <SHA256SUMS> [--require-all]

``attach`` compares the GitHub Release's existing assets with the files to attach, by name and
SHA-256, before anything is uploaded:

- an asset that is absent is uploaded;
- an asset that exists with the same bytes is kept as it is (a rerun after a partial or complete
  attachment);
- an asset that exists with other bytes, an asset that is not fully uploaded, and an asset this
  release does not produce are refused: the job fails and nothing is uploaded.

Existing assets are never replaced (no ``--clobber``), so a release can never end up holding
artifacts from two different builds. To recover from a refusal, delete the conflicting asset
from the release by hand, then rerun the job. After uploading, every asset is downloaded again
and checked against ``SHA256SUMS``.

``pypi-check`` compares the files already on PyPI for this version with the local
distributions. Any file there with other bytes, or any file there that this run did not build,
fails the job, so a later ``skip-existing`` upload only skips byte-identical files. With
``--require-all`` every local distribution must be on PyPI with the same SHA-256.

The distributions are exactly the wheels and sdist listed in ``SHA256SUMS``, and each must
still hash to its listed digest. The PyPI publish action writes a
``<distribution>.publish.attestation`` file next to each distribution it uploads with Trusted
Publishing; PyPI does not list those as files of the release, so they are ignored. Any other
file in the dist folder is refused, so nothing unlisted can reach the upload.
"""
from __future__ import annotations

import hashlib
import json
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


class GhRelease:
    """The `gh` operations used on one release. Tests replace it with an in-memory fake."""

    def __init__(self, tag: str):
        self.tag = tag

    def assets(self) -> list[dict]:
        out = subprocess.run(["gh", "release", "view", self.tag, "--json", "assets"],
                             check=True, capture_output=True, text=True).stdout
        return json.loads(out)["assets"]

    def download(self, name: str, dest: Path) -> Path:
        subprocess.run(["gh", "release", "download", self.tag, "--pattern", name,
                        "--dir", str(dest)], check=True)
        return dest / name

    def upload(self, paths: list[Path]) -> None:
        # No --clobber: an existing name makes gh fail instead of replacing the asset.
        subprocess.run(["gh", "release", "upload", self.tag, *map(str, paths)], check=True)


def plan_attach(release, files: list[Path]) -> tuple[list[Path], list[str], list[str]]:
    """Return (to upload, kept as identical, refusals) without changing the release."""
    by_name = {p.name: p for p in files}
    existing = {a["name"]: a for a in release.assets()}
    upload, kept, refused = [], [], []
    for name, asset in sorted(existing.items()):
        if name not in by_name:
            refused.append(f"{name}: on the release but not produced by this run")
        elif asset.get("state", "uploaded") != "uploaded":
            refused.append(f"{name}: an incomplete upload (state {asset.get('state')!r}); delete it and rerun")
    with tempfile.TemporaryDirectory() as tmp:
        for name, path in sorted(by_name.items()):
            if name not in existing:
                upload.append(path)
                continue
            if existing[name].get("state", "uploaded") != "uploaded":
                continue
            got = sha256(release.download(name, Path(tmp)))
            want = sha256(path)
            if got == want:
                kept.append(name)
            else:
                refused.append(f"{name}: the release holds other bytes ({got}) than this build ({want})")
    return upload, kept, refused


def attach(release, files: list[Path], sums: Path) -> None:
    upload, kept, refused = plan_attach(release, files)
    for name in kept:
        print(f"kept (identical): {name}")
    if refused:
        sys.exit("refusing to attach; nothing was uploaded:\n  " + "\n  ".join(refused))
    if upload:
        print("uploading: " + ", ".join(p.name for p in upload))
        release.upload(upload)
    # Read back every asset and check it against SHA256SUMS.
    want = read_sums(sums)
    names = {a["name"] for a in release.assets()}
    expected = {p.name for p in files}
    if names != expected:
        sys.exit(f"release assets {sorted(names)} are not {sorted(expected)}")
    with tempfile.TemporaryDirectory() as tmp:
        for name in sorted(want):
            got = sha256(release.download(name, Path(tmp)))
            if got != want[name]:
                sys.exit(f"{name}: released bytes {got} differ from SHA256SUMS {want[name]}")
    print(f"release assets verified: {len(expected)} files")


def pypi_files(name: str, version: str, opener=urllib.request.urlopen) -> dict[str, str] | None:
    """Filename -> SHA-256 of the files PyPI holds for this version, or None if it holds none."""
    url = f"https://pypi.org/pypi/{name}/{version}/json"
    try:
        with opener(url, timeout=60) as r:
            data = json.load(r)
    except urllib.error.HTTPError as e:
        if e.code == 404:
            return None
        raise
    return {f["filename"]: f["digests"]["sha256"] for f in data.get("urls", [])}


DIST_SUFFIXES = (".whl", ".tar.gz")
ATTESTATION_SUFFIX = ".publish.attestation"


def read_sums(sums: Path) -> dict[str, str]:
    """Filename -> SHA-256 from a ``sha256sum`` manifest."""
    want = {}
    for line in sums.read_text(encoding="utf-8").splitlines():
        if line.strip():
            digest, name = line.split(None, 1)
            want[name.strip().lstrip("*")] = digest.lower()
    return want


def local_distributions(dist: Path, sums: Path) -> dict[str, str]:
    """The distributions to check against PyPI: filename -> SHA-256, from ``SHA256SUMS``.

    Every listed file must be a wheel or sdist in ``dist`` with the listed digest. Every file in
    ``dist`` must be listed, or be the publish action's attestation for a listed distribution
    (ignored). Exits on any other case, before PyPI is contacted.
    """
    want = read_sums(sums)
    problems = []
    if not want:
        problems.append(f"{sums}: lists no distributions")
    for name, digest in sorted(want.items()):
        path = dist / name
        if not name.endswith(DIST_SUFFIXES) or Path(name).name != name:
            problems.append(f"{name}: listed in {sums.name} but not a wheel or sdist")
        elif not path.is_file():
            problems.append(f"{name}: listed in {sums.name} but not in {dist}")
        elif sha256(path) != digest:
            problems.append(f"{name}: local bytes ({sha256(path)}) differ from {sums.name} ({digest})")
    for p in sorted(dist.iterdir()):
        if p.name in want:
            continue
        if p.is_file() and p.name.endswith(ATTESTATION_SUFFIX) \
                and p.name[: -len(ATTESTATION_SUFFIX)] in want:
            continue  # written by the PyPI publish action; not a distribution
        problems.append(f"{p.name}: in {dist} but not a distribution listed in {sums.name}")
    if problems:
        sys.exit("PyPI check failed:\n  " + "\n  ".join(problems))
    return want


def check_pypi(local: dict[str, str], remote: dict[str, str] | None, require_all: bool) -> list[str]:
    remote = remote or {}
    problems = []
    for fn, digest in sorted(remote.items()):
        if fn not in local:
            problems.append(f"{fn}: on PyPI but not built by this run")
        elif local[fn] != digest:
            problems.append(f"{fn}: PyPI holds other bytes ({digest}) than this build ({local[fn]})")
    if require_all:
        problems += [f"{fn}: not on PyPI" for fn in sorted(local) if fn not in remote]
    return problems


def main(argv: list[str]) -> None:
    if len(argv) >= 4 and argv[0] == "attach":
        tag, dist, sums = argv[1], Path(argv[2]), Path(argv[3])
        files = sorted(p for p in dist.iterdir() if p.is_file()) + [sums]
        attach(GhRelease(tag), files, sums)
    elif len(argv) >= 3 and argv[0] == "pypi-check" and not argv[2].startswith("--"):
        import tomllib  # Python 3.11+; the release job runs 3.13

        dist, sums = Path(argv[1]), Path(argv[2])
        flags = argv[3:]
        if any(f != "--require-all" for f in flags):
            sys.exit(__doc__)
        require_all = "--require-all" in flags
        project = tomllib.loads((ROOT / "pyproject.toml").read_text(encoding="utf-8"))["project"]
        local = local_distributions(dist, sums)
        for attempt in range(6 if require_all else 1):
            problems = check_pypi(local, pypi_files(project["name"], project["version"]), require_all)
            # Wait only for files that are still missing; a mismatch never resolves.
            if not problems or not require_all or any("not on PyPI" not in x for x in problems):
                break
            time.sleep(20)  # the JSON API can lag behind an upload
        if problems:
            sys.exit("PyPI check failed:\n  " + "\n  ".join(problems))
        print(f"PyPI check passed ({'all present' if require_all else 'no conflicting files'})")
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main(sys.argv[1:])

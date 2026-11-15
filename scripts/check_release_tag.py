"""Release gate: the published GitHub Release's tag must be ``v`` + the synchronised version.

Run first in every release job (``python scripts/check_release_tag.py``). It checks that
``Cargo.toml`` (``workspace.package.version``) and ``pyproject.toml`` (``project.version``)
agree, that the workflow runs for a ``release`` event on a tag, and that the tag (the
checked-out ref and the release's own ``tag_name``) is ``v<version>``.
"""
import json
import os
import pathlib
import sys
import tomllib

root = pathlib.Path(__file__).resolve().parents[1]
crate = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"]
wheel = tomllib.loads((root / "pyproject.toml").read_text(encoding="utf-8"))["project"]["version"]
errors = []
if crate != wheel:
    errors.append(f"Cargo.toml {crate} != pyproject.toml {wheel}")
event = os.environ.get("GITHUB_EVENT_NAME")
if event != "release":
    errors.append(f"expected a release event, got {event!r}")
if os.environ.get("GITHUB_REF_TYPE") != "tag":
    errors.append(f"expected a tag ref, got {os.environ.get('GITHUB_REF_TYPE')!r}")
ref = os.environ.get("GITHUB_REF_NAME")
if ref != f"v{crate}":
    errors.append(f"ref {ref!r} is not v{crate}")
path = os.environ.get("GITHUB_EVENT_PATH")
if path:
    release = json.loads(pathlib.Path(path).read_text(encoding="utf-8")).get("release") or {}
    if release.get("tag_name") != f"v{crate}":
        errors.append(f"release tag {release.get('tag_name')!r} is not v{crate}")
    if release.get("draft"):
        errors.append("the release is still a draft")
if errors:
    sys.exit("release gate failed: " + "; ".join(errors))
print(f"release gate passed: v{crate}")

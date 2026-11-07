"""Verify SOURCE_MANIFEST.json: the SHA-256 of every published file.

    python scripts/verify_sources.py           # fails if any file differs, is missing or is extra
    python scripts/verify_sources.py --write   # rewrite the manifest after an intentional change

Text files are hashed with CRLF read as LF, so a Windows checkout verifies too. Files with a
NUL byte are hashed as stored. MIGRATION.json is the immutable receipt of the 0.1.0 extraction
from the private development repository; this script checks that it is well formed and that the
versions in Cargo.toml and pyproject.toml agree.
"""
from pathlib import Path
import argparse
import hashlib
import json
import tomllib

root = Path(__file__).resolve().parents[1]
SCHEMA = "ras-commander-fast-dss-source/1"
SKIP_DIRS = {".git", "target", "dist", "working", ".venv", ".pytest_cache", "__pycache__", "pkg", "node_modules"}
SKIP_SUFFIXES = {".pyc", ".pyd", ".so", ".dll", ".pdb", ".dylib"}
SKIP_FILES = {"SOURCE_MANIFEST.json"}


def digest(path):
    data = path.read_bytes()
    if b"\0" not in data:
        data = data.replace(b"\r\n", b"\n")
    return hashlib.sha256(data).hexdigest()


def published():
    files = []
    for path in root.rglob("*"):
        rel = path.relative_to(root)
        if not path.is_file() or set(rel.parts[:-1]) & SKIP_DIRS or path.suffix in SKIP_SUFFIXES:
            continue
        if rel.as_posix() in SKIP_FILES or rel.parts[0].startswith("venv"):
            continue
        files.append(path)
    return {p.relative_to(root).as_posix(): digest(p) for p in sorted(files, key=lambda p: p.relative_to(root).as_posix())}


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--write", action="store_true")
    args = parser.parse_args()
    files = published()
    target = root / "SOURCE_MANIFEST.json"
    if args.write:
        target.write_text(json.dumps({"schema": SCHEMA, "files": files}, indent=2) + "\n", encoding="utf-8", newline="\n")
    else:
        recorded = json.loads(target.read_text(encoding="utf-8"))
        assert recorded["schema"] == SCHEMA, recorded["schema"]
        want = recorded["files"]
        missing = sorted(set(want) - set(files))
        extra = sorted(set(files) - set(want))
        changed = sorted(k for k in set(want) & set(files) if want[k] != files[k])
        assert not (missing or extra or changed), f"Source manifest is stale: missing {missing[:10]}, extra {extra[:10]}, changed {changed[:10]}"
    workspace = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))["workspace"]["package"]["version"]
    project = tomllib.loads((root / "pyproject.toml").read_text(encoding="utf-8"))["project"]["version"]
    assert workspace == project, f"Cargo.toml {workspace} != pyproject.toml {project}"
    migration = json.loads((root / "MIGRATION.json").read_text(encoding="utf-8"))
    assert migration["schema"] == "ras-commander-fast-dss-extraction/1"
    assert len(migration["source_commit"]) == 40
    print(f"Verified {len(files)} distribution sources at version {workspace}; extraction receipt retained")


if __name__ == "__main__":
    main()

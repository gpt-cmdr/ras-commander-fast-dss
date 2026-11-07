"""Write the licence bundle that ships inside the wheel.

    python -I scripts/third_party_licenses.py

Covers every crate linked into the wheel with all features on, on every shipped target, so it
stays correct when `vortex` is enabled. Run it after any change to Cargo.lock and commit the
result; crates/dss-vortex/tests/build_time_code.rs fails if the bundle's crate list is stale.
Licence texts come from each crate's own files, and identical texts are stored once.
"""
import hashlib
import json
import re
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "python" / "ras_commander_fast_dss" / "THIRD_PARTY_LICENSES.txt"
TARGETS = [
    "x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc",
    "x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin", "aarch64-apple-darwin",
]
LICENSE_FILE = re.compile(r"^(LICEN[CS]E|COPYING|NOTICE|UNLICENSE|COPYRIGHT)", re.I)


def linked_packages():
    args = ["cargo", "metadata", "--format-version", "1", "--locked", "--all-features"]
    for target in TARGETS:
        args += ["--filter-platform", target]
    meta = json.loads(subprocess.run(args, cwd=ROOT, check=True, capture_output=True).stdout)
    by_id = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    stack = [p["id"] for p in meta["packages"] if p["name"] == "dss-py"]
    seen = set()
    while stack:
        node = stack.pop()
        if node in seen:
            continue
        seen.add(node)
        for dep in nodes[node]["deps"]:
            if any(kind["kind"] is None for kind in dep["dep_kinds"]):
                stack.append(dep["pkg"])
    # Registry crates, and the readers vendored in-tree with CLB's patches (vendor/README.md).
    vendored = str(ROOT / "vendor")
    packages = [by_id[i] for i in seen if by_id[i]["source"] or by_id[i]["manifest_path"].startswith(vendored)]
    return sorted(packages, key=lambda p: (p["name"], p["version"]))


def license_files(package):
    root = Path(package["manifest_path"]).parent
    files = [p for p in root.iterdir() if p.is_file() and LICENSE_FILE.match(p.name)]
    for directory in ("LICENSES", "licenses", "license"):
        if (root / directory).is_dir():
            files += [p for p in (root / directory).iterdir() if p.is_file()]
    if package.get("license_file"):
        files.append(root / package["license_file"])
    return sorted(set(files))


def main():
    index, texts, without_text = [], {}, []
    # A crate may publish no licence file. Its SPDX expression still names the terms; for these
    # we state the authors and point at the standard text of the Apache-2.0 option.
    apache = None
    for package in linked_packages():
        ids = []
        for path in license_files(package):
            text = path.read_bytes().decode("utf-8", "replace").replace("\r\n", "\n").strip() + "\n"
            key = hashlib.sha256(text.encode()).hexdigest()[:12]
            texts[key] = text
            if apache is None and path.name.upper().startswith("LICENSE-APACHE"):
                if "Apache License" in text and "Version 2.0" in text:
                    apache = key
            ids.append(key)
        spdx = package.get("license") or "see crate"
        if not ids:
            without_text.append((len(index), package, spdx))
        index.append(f"crate {package['name']} {package['version']} | {spdx} | {', '.join(ids)}")
    for position, package, spdx in without_text:
        if "Apache-2.0" not in spdx or apache is None:
            raise SystemExit(f"{package['name']}: no licence file and no Apache-2.0 option: {spdx}")
        authors = "; ".join(package["authors"]) or "not stated"
        index[position] += f"{apache} (crate ships no licence file; authors: {authors})"

    parts = [
        "Third-party Rust crates that may be linked into this wheel, with their licences.\n"
        "The list includes crates linked only when the optional vortex feature is enabled.\n"
        "Each line gives the crate, its SPDX expression, and the licence texts below.\n\n",
        "\n".join(index),
        "\n",
    ]
    # Source ported into this repository, which no crate list can see (AOCL-LibM, ARM
    # optimized-routines, HEC-DSS). The NOTICE travels whole.
    notice = (ROOT / "NOTICE").read_text(encoding="utf-8").replace("\r\n", "\n").strip()
    parts.append(f"\n{'=' * 78}\nNOTICE\n{'=' * 78}\n\n{notice}\n")
    for key in sorted(texts):
        parts.append(f"\n{'=' * 78}\nLicence text {key}\n{'=' * 78}\n\n{texts[key]}")
    OUT.write_bytes("".join(parts).encode("utf-8"))
    print(f"{len(index)} crates, {len(texts)} texts, {OUT.stat().st_size} bytes")
    for _, package, spdx in without_text:
        print(f"no licence file in crate: {package['name']} {package['version']} ({spdx})")


if __name__ == "__main__":
    main()

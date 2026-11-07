"""The built Zarr reader, default Limits, under a Windows Job Object's process commit limit.

Usage: python -I zarr_job.py <zarr_dump.exe> <probe-dir> <out-dir> [limit-MiB] [zarr_dump arguments]

Each folder in <probe-dir> is one store. `nc_job.py`'s equivalent for stores, which are folders:
it runs each in a process of its own through that file's `run`, and reports the exit code, peak
committed memory and what the reader said. Exit 1 if a probe ends in anything but exit 0 with a dump.
"""
import importlib.util
import json
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent


def nc_job():
    spec = importlib.util.spec_from_file_location("nc_job", HERE / "nc_job.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main():
    tool, probes, out = sys.argv[1], Path(sys.argv[2]), Path(sys.argv[3])
    limit = int(sys.argv[4]) if len(sys.argv) > 4 else 128
    job = nc_job()
    failed = 0
    probed = sorted(p for p in probes.iterdir() if p.is_dir())
    if not probed:
        print("no probes in", probes)
        failed += 1
    for probe in probed:
        code, peak, _stdout, stderr = job.run(tool, probe, out / probe.name, limit << 20, sys.argv[5:])
        said = ""
        dump = out / probe.name / "dtos.json"
        if dump.is_file():
            data = json.loads(dump.read_text(encoding="utf-8"))
            said = "; ".join(str(r.get("error", "read"))[:110] for r in data.get("variables", [])[:3])
            said = said or data.get("file_error", "no variables")[:110]
        else:
            said = "no dump"
            failed += code == 0
        print(f"{probe.name[:36]:36} exit={code:#x} peak={peak / (1 << 20):7.1f} MiB {said} {stderr[:120]}")
        failed += code != 0
    print("RESULT", "FAIL" if failed else "PASS", failed, "of", len(probed))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())

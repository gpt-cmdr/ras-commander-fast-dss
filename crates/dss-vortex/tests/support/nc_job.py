"""The built reader, default Limits, under a Windows Job Object's process commit limit.

Usage: python -I nc_job.py <nc_dump.exe> <probe-dir> <out-dir> [limit-MiB] [nc_dump arguments]

A long record needs `--max-steps`: without it `nc_dump` writes every step, and its own JSON grows in
memory, which is the harness and not the reader.

Reports each probe's exit code,
peak committed memory and what the reader said. Exit 1 if a probe ends in anything but exit 0.
"""
import ctypes as c
import json
import subprocess
import sys
from ctypes import wintypes as w
from pathlib import Path

k = c.WinDLL("kernel32", use_last_error=True)


class BasicLimits(c.Structure):
    _fields_ = [("PerProcessUserTimeLimit", c.c_longlong), ("PerJobUserTimeLimit", c.c_longlong),
                ("LimitFlags", w.DWORD), ("MinimumWorkingSetSize", c.c_size_t),
                ("MaximumWorkingSetSize", c.c_size_t), ("ActiveProcessLimit", w.DWORD),
                ("Affinity", c.c_size_t), ("PriorityClass", w.DWORD), ("SchedulingClass", w.DWORD)]


class IoCounters(c.Structure):
    _fields_ = [(n, c.c_ulonglong) for n in ("r", "w", "o", "rb", "wb", "ob")]


class ExtendedLimits(c.Structure):
    _fields_ = [("BasicLimitInformation", BasicLimits), ("IoInfo", IoCounters),
                ("ProcessMemoryLimit", c.c_size_t), ("JobMemoryLimit", c.c_size_t),
                ("PeakProcessMemoryUsed", c.c_size_t), ("PeakJobMemoryUsed", c.c_size_t)]


JOB_OBJECT_LIMIT_PROCESS_MEMORY = 0x100
EXTENDED = 9
k.CreateJobObjectW.restype = w.HANDLE
k.CreateJobObjectW.argtypes = [c.c_void_p, w.LPCWSTR]
k.SetInformationJobObject.argtypes = [w.HANDLE, c.c_int, c.c_void_p, w.DWORD]
k.QueryInformationJobObject.argtypes = [w.HANDLE, c.c_int, c.c_void_p, w.DWORD, c.c_void_p]
k.AssignProcessToJobObject.argtypes = [w.HANDLE, w.HANDLE]


def run(tool, probe, out, limit, extra=()):
    job = k.CreateJobObjectW(None, None)
    info = ExtendedLimits()
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY
    info.ProcessMemoryLimit = limit
    if not k.SetInformationJobObject(job, EXTENDED, c.byref(info), c.sizeof(info)):
        raise OSError("SetInformationJobObject")
    # Suspended start would be stricter; the first allocations are small, so assign straight away.
    p = subprocess.Popen([str(tool), str(probe), str(out), *extra], stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                         text=True, creationflags=0x00000004)
    k.AssignProcessToJobObject(job, int(p._handle))
    resume(p.pid)
    stdout, stderr = p.communicate()
    k.QueryInformationJobObject(job, EXTENDED, c.byref(info), c.sizeof(info), None)
    k.CloseHandle(job)
    return p.returncode, info.PeakProcessMemoryUsed, stdout.strip(), stderr.strip()


def resume(pid):
    k.CreateToolhelp32Snapshot.restype = w.HANDLE

    class ThreadEntry(c.Structure):
        _fields_ = [("dwSize", w.DWORD), ("cntUsage", w.DWORD), ("th32ThreadID", w.DWORD),
                    ("th32OwnerProcessID", w.DWORD), ("tpBasePri", w.LONG), ("tpDeltaPri", w.LONG),
                    ("dwFlags", w.DWORD)]

    snapshot = k.CreateToolhelp32Snapshot(4, 0)
    entry = ThreadEntry()
    entry.dwSize = c.sizeof(entry)
    more = k.Thread32First(snapshot, c.byref(entry))
    k.OpenThread.restype = w.HANDLE
    while more:
        if entry.th32OwnerProcessID == pid:
            thread = k.OpenThread(2, False, entry.th32ThreadID)
            k.ResumeThread(thread)
            k.CloseHandle(thread)
        more = k.Thread32Next(snapshot, c.byref(entry))
    k.CloseHandle(snapshot)


def main():
    tool, probes, out = sys.argv[1], Path(sys.argv[2]), Path(sys.argv[3])
    limit = int(sys.argv[4]) if len(sys.argv) > 4 else 128
    failed = 0
    probed = sorted(probes.iterdir())
    if not probed:
        print("no probes in", probes)
        failed += 1
    for probe in probed:
        # The reader's own error for a file it cannot open is a typed one, with exit 0, and says
        # nothing about the file. So a probe it cannot read is a failed probe.
        try:
            with open(probe, "rb") as opened:
                opened.read(1)
        except OSError as error:
            print(f"{probe.name[:32]:32} cannot be read: {error}")
            failed += 1
            continue
        code, peak, stdout, stderr = run(tool, probe, out / probe.name, limit << 20, sys.argv[5:])
        said = ""
        dump = out / probe.name / "dtos.json"
        if dump.is_file():
            data = json.loads(dump.read_text(encoding="utf-8"))
            said = "; ".join(str(r.get("error", "read"))[:110] for r in data.get("variables", [])[:3])
            said = said or data.get("file_error", "no variables")[:110]
        elif code == 0:
            said = "exit 0 with no dump"
            failed += 1
        print(f"{probe.name[:32]:32} {probe.stat().st_size:9} exit={code:#x} peak={peak / (1 << 20):7.1f} MiB {said} {stderr[:120]}")
        failed += code != 0
    print("RESULT", "FAIL" if failed else "PASS", failed)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())

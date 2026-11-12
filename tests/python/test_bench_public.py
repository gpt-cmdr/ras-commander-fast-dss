"""Tests of benchmarks/bench_public.py: a quick smoke run (engine only) and its failure paths.

The failure-path tests load the script in this process and replace only the worker launch, so a
worker error, a nonzero worker exit, a timeout and a corrupt write are simulated without touching
the tools.
"""
import functools
import importlib.util
import json
import subprocess
import sys
import tempfile
from pathlib import Path

import pytest

SCRIPT = Path(__file__).resolve().parents[2] / "benchmarks" / "bench_public.py"
pytestmark = pytest.mark.skipif(not SCRIPT.is_file(), reason="benchmarks/bench_public.py is not in this tree")

# Strings an exception might carry. None of them may be published: Windows, UNC and POSIX paths,
# root-level paths, an ambiguous six-directory upper-case path, and valid DSS pathnames in both
# spellings (which the script never needs to extract from error text).
LEAKS = [
    r"Z:\example-install\person\native.dll",
    "/home/alice/venv/lib/libdss.so",
    r"\\fileserver\share\bob\heclib.dll",
    "/secret",
    "/home/Example User/lib/native.so",
    "/HOME/OTHERUSER/PRIVATE/BUILD/LIB/NATIVE/",
    "/Bench/Loc/Flow//1Hour/Synth/",
    "/BENCH/LOC/FLOW//1HOUR/SYNTH/",
]
# Private prose that a version or metadata field might carry.
PROSE = ["Client Private Project", "1.26.4+PRIVATE_CLIENT"]
LEAK_WORDS = ["PRIVATE_CLIENT", "Client Private", "example-install", "person", "alice", "fileserver", "bob", "secret", "Example User", "OTHERUSER",
              "PRIVATE", "Bench/Loc", "BENCH/LOC/FLOW", "native.so", "native.dll", "libdss", "heclib.dll"]


def load():
    spec = importlib.util.spec_from_file_location("bench_public_under_test", SCRIPT)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def report(out):
    text = (out / "bench_public.json").read_text(encoding="utf-8")
    md = (out / "bench_public.md").read_text(encoding="utf-8")
    return json.loads(text), text, md


def no_paths(text, *dirs):
    for d in dirs:
        for p in (str(d), str(d).replace("\\", "/"), str(d).replace("\\", "\\\\")):
            assert p not in text


def test_bench_public_quick_engine(tmp_path):
    out = tmp_path / "results"
    cp = subprocess.run(
        [sys.executable, "-I", str(SCRIPT), "--quick", "--tools", "engine", "--reps", "1", "--versions", "7",
         "--kinds", "pd", "--no-fixtures", "--out", str(out)],
        capture_output=True, text=True, timeout=600, check=False,
    )
    assert cp.returncode == 0, cp.stdout[-3000:] + cp.stderr[-3000:]
    rep, text, md = report(out)
    assert rep["outcome"]["verified_all"] is True
    assert rep["outcome"]["error"] == rep["outcome"]["mismatch"] == 0
    assert rep["tools"]["engine"]["ras-commander-fast-dss"]
    writes = [r for r in rep["rows"] if r["op"] == "write"]
    assert {(r["mode"], r["width"]) for r in writes} == {
        (m, w) for m in ("engine", "engine_none", "engine_session", "engine_session_none")
        for w in ("float32", "float64")}
    for r in writes:  # every repetition (warm-up included) verified
        assert r["status"] == "pass" and r["files_written"] == r["files_verified_equal"] == 2
    assert all(r["status"] == "pass" and r["sync"] and r["dataset"] for r in rep["rows"])
    assert {r["sync"].split(":")[0] for r in writes} == {'sync="end"', 'sync="none"'}
    for d in (tmp_path, Path(tempfile.gettempdir()), Path.home()):
        no_paths(text + md + cp.stdout + cp.stderr, d)
    assert md.startswith("# HEC-DSS benchmark") and "**Correctness:** all" in md


def _patched_launch(mod, transform):
    """Replace Runner._launch: ``transform(job, out_path, real)`` decides what happens."""
    original = mod.Runner._launch

    def launch(self, cmd, timeout):
        job_path, out_path = Path(cmd[-2]), Path(cmd[-1])
        job = json.loads(job_path.read_text(encoding="utf-8"))
        return transform(job, out_path, lambda: original(self, cmd, timeout))

    mod.Runner._launch = launch


QUICK_READ = ["--quick", "--tools", "engine", "--reps", "1", "--versions", "7", "--kinds", "pd", "--ops", "read",
              "--no-fixtures"]


def test_worker_error_fails_the_run_and_publishes_no_error_text(tmp_path, capsys):
    """Exception text, a non-identifier class name and odd version strings never reach the report."""
    mod = load()

    def transform(job, out_path, real):
        cp = real()
        if job["op"] == "read_pd":
            res = json.loads(out_path.read_text(encoding="utf-8"))
            res["error"] = "OSError: failed at " + " and ".join(LEAKS)
            res["error_type"] = LEAKS[3]
            res["versions"] = {"ras-commander-fast-dss": LEAKS[5], LEAKS[0]: "1.0"}
            out_path.write_text(json.dumps(res), encoding="utf-8")
        return cp

    _patched_launch(mod, transform)
    out, diag = tmp_path / "res", tmp_path / "diagnostics.txt"
    code = mod.main(QUICK_READ + ["--out", str(out), "--diagnostics", str(diag)])
    captured = capsys.readouterr()
    assert code == 4
    rep, text, md = report(out)
    assert rep["outcome"]["verified_all"] is False and rep["outcome"]["error"] == 1
    (row,) = rep["rows"]
    assert row["status"] == "error" and row["dataset"] == "pd_v7.dss"
    (f,) = row["failures"]
    assert f == {"category": "tool_exception", "message": mod.FAILURE_TEXT["tool_exception"],
                 "error_class": "UnrecognizedError", "detail": None}
    assert "RUN FAILED" in md and "**Correctness:** all" not in md and "tool_exception" in md
    everything = text + md + captured.out + captured.err
    for w in LEAKS + LEAK_WORDS:
        assert w not in everything, w
    raw = diag.read_text(encoding="utf-8")  # the raw text is kept, locally only
    assert "NOT FOR PUBLISHING" in raw and LEAKS[3] in raw and LEAKS[5] in raw


def test_nonzero_worker_exit_after_a_result_is_an_error(tmp_path):
    """A real worker writes a complete result, then exits 9 (as a crash at native or JVM shutdown)."""
    mod = load()
    original = mod.Runner._launch

    def launch(self, cmd, timeout):
        job = json.loads(Path(cmd[-2]).read_text(encoding="utf-8"))
        if job["op"] != "read_pd":
            return original(self, cmd, timeout)
        code = "\n".join([
            "import os, runpy, sys",
            f"sys.argv = {cmd[2:]!r}",
            "try:",
            f"    runpy.run_path({str(SCRIPT)!r}, run_name='__main__')",
            "except SystemExit:",
            "    pass",
            "os._exit(9)",
        ])
        cp = subprocess.run([sys.executable, "-I", "-c", code], capture_output=True, text=True, timeout=timeout,
                            check=False)
        assert cp.returncode == 9 and json.loads(Path(cmd[-1]).read_text(encoding="utf-8"))["digest"]
        return cp

    mod.Runner._launch = launch
    out = tmp_path / "res"
    code = mod.main(QUICK_READ + ["--out", str(out)])
    assert code == 4
    rep, _, md = report(out)
    (row,) = rep["rows"]
    assert row["status"] == "error"
    assert [f["category"] for f in row["failures"]] == ["nonzero_exit"]
    assert row["failures"][0]["detail"] == "exit status 9"
    assert row["warm_median_s"] is not None and row["reps"] == 1  # measurements kept
    assert rep["outcome"]["verified_all"] is False and "RUN FAILED" in md and "**Correctness:** all" not in md


def test_worker_timeout_is_a_failed_cell_and_the_report_is_written(tmp_path):
    mod = load()

    def transform(job, out_path, real):
        if job["op"] == "read_pd":
            raise subprocess.TimeoutExpired(cmd="worker", timeout=7)
        return real()

    _patched_launch(mod, transform)
    out = tmp_path / "res"
    code = mod.main(QUICK_READ + ["--out", str(out), "--timeout", "7"])
    assert code == 4
    rep, _, md = report(out)
    (row,) = rep["rows"]
    assert row["status"] == "error"
    assert row["failures"][0]["category"] == "timeout" and row["failures"][0]["detail"] == "after 7 s"
    assert "RUN FAILED" in md


def test_corrupt_earlier_repetition_is_detected(tmp_path):
    mod = load()

    def transform(job, out_path, real):
        cp = real()
        if job["op"] == "write":
            warmup = Path(json.loads(out_path.read_text(encoding="utf-8"))["written"][0])
            warmup.write_bytes(b"not a DSS file")
        return cp

    _patched_launch(mod, transform)
    out = tmp_path / "res"
    code = mod.main(["--quick", "--tools", "engine", "--reps", "2", "--versions", "7", "--kinds", "pd",
                     "--ops", "write", "--write-modes", "engine_session_none", "--widths", "float64",
                     "--out", str(out)])
    assert code == 3
    rep, _, md = report(out)
    (row,) = rep["rows"]
    assert row["status"] == "mismatch" and row["files_verified_equal"] == 2
    assert row["problems"][0].startswith("warm-up (engine_session_none_pd_float64_v7_0.dss):")
    assert "RUN FAILED" in md


def test_load_guard_refuses_when_cpu_cannot_be_measured(tmp_path):
    mod = load()
    mod.system_cpu_percent = lambda seconds=2.0: None
    out = tmp_path / "res"
    code = mod.main(QUICK_READ + ["--out", str(out), "--max-cpu", "25"])
    assert code == 5
    rep, _, md = report(out)
    assert rep["rows"] == [] and rep["outcome"]["fatal"][0]["status"] == "refused"
    assert rep["load_guard"]["applied"] is False
    assert "RUN FAILED" in md

    out2 = tmp_path / "res2"
    code = mod.main(QUICK_READ + ["--out", str(out2), "--max-cpu", "25", "--allow-unmeasured-load"])
    assert code == 0
    rep, _, md = report(out2)
    assert rep["load_guard"]["applied"] is False and "NOT applied" in md


def test_published_text_helpers():
    mod = load()
    for name in LEAKS + ["a b", "", None, "x" * 65]:
        assert mod.class_name(name) == "UnrecognizedError"
    assert mod.class_name("JavaException") == "JavaException"
    for v in LEAKS + PROSE[:1] + ["", None, "v", "1.2 beta", "1.0/2", "PRIVATE_CLIENT"]:
        assert mod.version_text(v) == "unspecified", v
    for v in ("0.1.2", "17.0.11", "3.1.0", "2.0.0rc1", "1.0.post1.dev3", "1!2.0"):
        assert mod.version_text(v) == v
    assert mod.version_text("1.26.4+PRIVATE_CLIENT") == "1.26.4"  # local label removed
    assert mod.version_text("7-IU", "heclib") == "7-IU" and mod.version_text("7-IU") == "unspecified"
    assert mod.public_versions({"numpy": "1.26.4+x", "Client Private Project": "1.0", "java": "17"}) == {
        "numpy": "1.26.4", "java": "17"}
    # No CPU-model validator remains: the model is never published, whatever it looks like.
    assert not hasattr(mod, "cpu_text") and not hasattr(mod, "cpu_model")
    assert mod.CPU_MODEL_NOT_RECORDED == "not recorded"
    assert mod.release_text("10.0.19045") == "10.0.19045" and mod.release_text("#1 SMP x") == "unspecified"
    mod._user = lambda: "alice"
    mod._host = lambda: "build-host-7"
    assert mod.scrub("user alice on build-host-7 (Alice)") == "user <user> on <host> (<user>)"
    assert mod.scrub_tree({"a": ["alice"], "n": 3}) == {"a": ["<user>"], "n": 3}
    f = mod.failure("timeout", LEAKS[0], "after 7 s")
    assert f["error_class"] == "UnrecognizedError"
    assert mod.failure_text(f) == "the worker timed out and was stopped (after 7 s) [UnrecognizedError]"


def test_machine_metadata_and_versions_publish_only_validated_fields(tmp_path, capsys, monkeypatch):
    """Injected registry (or OS) CPU text, platform strings and tool versions never reach the report."""
    import platform

    mod = load()
    raw = " | ".join(LEAKS[3:]) + " " + PROSE[0]
    if sys.platform == "win32":
        import winreg

        real_query = winreg.QueryValueEx

        def query(key, name):
            return (raw, winreg.REG_SZ) if name == "ProcessorNameString" else real_query(key, name)

        monkeypatch.setattr(winreg, "QueryValueEx", query)
    else:  # the OS source differs; inject at the raw-source boundary
        monkeypatch.setattr(mod, "_cpu_model_raw", lambda: raw)
    monkeypatch.setattr(platform, "system", lambda: raw)
    monkeypatch.setattr(platform, "release", lambda: LEAKS[3])
    monkeypatch.setattr(platform, "version", lambda: PROSE[1])
    monkeypatch.setattr(platform, "machine", lambda: LEAKS[5])
    monkeypatch.setattr(platform, "python_implementation", lambda: PROSE[0])

    def transform(job, out_path, real):
        cp = real()
        if job["op"] == "read_pd":
            res = json.loads(out_path.read_text(encoding="utf-8"))
            res["versions"] = {"ras-commander-fast-dss": PROSE[0], "numpy": PROSE[1], PROSE[0]: "1.0",
                               LEAKS[3]: "2.0", "java": LEAKS[5]}
            out_path.write_text(json.dumps(res), encoding="utf-8")
        return cp

    _patched_launch(mod, transform)
    out = tmp_path / "PRIVATE_CLIENT_OUT"
    code = mod.main(QUICK_READ + ["--out", str(out)])
    captured = capsys.readouterr()
    assert code == 0
    rep, text, md = report(out)
    m = rep["machine"]
    assert (m["os"], m["os_release"], m["os_version"], m["architecture"], m["cpu_model"],
            m["python_implementation"]) == ("other", "unspecified", "unspecified", "other", "not recorded", "other")
    assert m["python"] == "{}.{}.{}".format(*sys.version_info[:3])
    assert rep["tools"]["engine"] == {"ras-commander-fast-dss": "unspecified", "numpy": "1.26.4",
                                      "java": "unspecified"}
    everything = text + md + captured.out + captured.err
    for w in LEAKS + PROSE + LEAK_WORDS + ["PRIVATE_CLIENT_OUT"]:
        assert w not in everything, w


# Prose only, with nothing a character pattern could reject: the injected CPU model must still never
# be published. The second is a plain label with no punctuation at all.
CPU_PROSE = ["Client Private Project", "Riverside Levee Board"]


def _unavailable(*a, **k):
    raise OSError("unavailable")


def _route_registry(mod, monkeypatch, tmp_path, prose):
    import winreg

    real_query = winreg.QueryValueEx
    monkeypatch.setattr(winreg, "QueryValueEx",
                        lambda key, name: (prose, winreg.REG_SZ) if name == "ProcessorNameString"
                        else real_query(key, name))


def _route_proc_cpuinfo(mod, monkeypatch, tmp_path, prose):
    cpuinfo = tmp_path / "cpuinfo"
    cpuinfo.write_text(f"processor\t: 0\nvendor_id\t: GenuineIntel\nmodel name\t: {prose}\n", encoding="utf-8")
    monkeypatch.setattr(mod, "CPUINFO", cpuinfo)
    monkeypatch.setattr(mod, "_cpu_model_raw", functools.partial(mod._cpu_model_raw, "linux"))


def _route_sysctl(mod, monkeypatch, tmp_path, prose):
    real_run = subprocess.run

    def run(cmd, *a, **k):  # only the sysctl call is answered; worker launches stay real
        if isinstance(cmd, list) and cmd[:1] == ["sysctl"]:
            return subprocess.CompletedProcess(cmd, 0, stdout=prose + "\n", stderr="")
        return real_run(cmd, *a, **k)

    monkeypatch.setattr(mod.subprocess, "run", run)
    monkeypatch.setattr(mod, "_cpu_model_raw", functools.partial(mod._cpu_model_raw, "darwin"))


def _route_platform_processor(mod, monkeypatch, tmp_path, prose):
    import platform

    monkeypatch.setattr(platform, "processor", lambda: prose)
    monkeypatch.setattr(mod, "_cpu_model_raw", functools.partial(mod._cpu_model_raw, "sunos5"))


def _route_processor_identifier(mod, monkeypatch, tmp_path, prose):
    """Registry and WMI unavailable: platform.processor() returns PROCESSOR_IDENTIFIER."""
    import platform
    import winreg

    monkeypatch.setenv("PROCESSOR_IDENTIFIER", prose)
    monkeypatch.setattr(winreg, "QueryValueEx", _unavailable)
    if hasattr(platform, "_wmi_query"):
        monkeypatch.setattr(platform, "_wmi_query", _unavailable)
    monkeypatch.setattr(platform, "_uname_cache", None, raising=False)


def _route_wmi(mod, monkeypatch, tmp_path, prose):
    """Registry unavailable: platform.processor() reads the CPU caption and manufacturer from WMI."""
    import platform
    import winreg

    real_wmi = platform._wmi_query

    def wmi(table, *keys):
        if table == "CPU" and keys == ("Manufacturer", "Caption"):
            return CPU_PROSE[1], prose
        return real_wmi(table, *keys)

    monkeypatch.setattr(winreg, "QueryValueEx", _unavailable)
    monkeypatch.setattr(platform, "_wmi_query", wmi)
    monkeypatch.setattr(platform, "_uname_cache", None, raising=False)


_WIN = pytest.mark.skipif(sys.platform != "win32", reason="a Windows-only CPU-model route")
CPU_ROUTES = [
    pytest.param(_route_registry, id="registry", marks=_WIN),
    pytest.param(_route_proc_cpuinfo, id="proc_cpuinfo"),
    pytest.param(_route_sysctl, id="sysctl"),
    pytest.param(_route_platform_processor, id="platform_processor"),
    pytest.param(_route_processor_identifier, id="processor_identifier", marks=_WIN),
    pytest.param(_route_wmi, id="wmi", marks=[
        _WIN, pytest.mark.skipif(not hasattr(__import__("platform"), "_wmi_query"),
                                 reason="this Python reads no WMI in platform.processor()")]),
]


@pytest.mark.parametrize("prose", CPU_PROSE)
@pytest.mark.parametrize("route", CPU_ROUTES)
def test_injected_cpu_prose_is_never_published(route, prose, tmp_path, capsys, monkeypatch):
    """Prose returned by any CPU-model source reaches only the local --diagnostics file."""
    mod = load()
    route(mod, monkeypatch, tmp_path, prose)
    assert prose in mod._cpu_model_raw()  # the route really yields the injected text
    out, diag = tmp_path / "res", tmp_path / "diagnostics.txt"
    code = mod.main(QUICK_READ + ["--out", str(out), "--diagnostics", str(diag)])
    captured = capsys.readouterr()
    assert code == 0
    rep, text, md = report(out)
    m = rep["machine"]
    assert m["cpu_model"] == "not recorded"
    assert m["architecture"] in ("x86_64", "arm64", "x86", "arm", "other")
    assert isinstance(m["logical_cores"], int) and not isinstance(m["logical_cores"], bool)
    assert m["physical_cores"] is None or isinstance(m["physical_cores"], int)
    everything = text + md + captured.out + captured.err
    for w in CPU_PROSE + prose.split():
        assert w not in everything, w
    raw = diag.read_text(encoding="utf-8")  # the raw model is kept, locally only
    assert "NOT FOR PUBLISHING" in raw and prose in raw


@pytest.mark.parametrize("prose", CPU_PROSE)
def test_report_without_diagnostics_reads_no_cpu_model(prose, tmp_path, capsys, monkeypatch):
    """Without --diagnostics the OS-reported model is not even read."""
    mod = load()
    calls = []
    monkeypatch.setattr(mod, "_cpu_model_raw", lambda *a: calls.append(a) or prose)
    out = tmp_path / "res"
    code = mod.main(QUICK_READ + ["--out", str(out)])
    captured = capsys.readouterr()
    assert code == 0 and calls == []
    rep, text, md = report(out)
    assert rep["machine"]["cpu_model"] == "not recorded"
    for w in [prose] + prose.split():
        assert w not in text + md + captured.out + captured.err, w


@pytest.mark.parametrize("args", [
    ["--kinds", "/secret"], ["--tools", "/HOME/OTHERUSER/PRIVATE/BUILD/LIB/NATIVE/"], ["--python", "/secret"],
    ["--python", "Client Private Project=x"], ["--reps", "PRIVATE_CLIENT"], ["/secret"],
    ["--versions", "1.26.4+PRIVATE_CLIENT"],
])
def test_usage_errors_print_a_fixed_message(args):
    cp = subprocess.run([sys.executable, "-I", str(SCRIPT), *args], capture_output=True, text=True, timeout=120,
                        check=False)
    assert cp.returncode == 2
    assert cp.stderr.startswith("bench_public.py: error: ") and cp.stderr.rstrip().endswith("see --help")
    for w in LEAKS + PROSE + ["secret", "OTHERUSER", "PRIVATE"]:
        assert w not in cp.stdout + cp.stderr, w

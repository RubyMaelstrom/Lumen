#!/usr/bin/env python3
"""Run Lumen's pinned, offline, interleaved engine benchmark matrix.

The JSON report is checkpointed after every child process so a long run retains its raw evidence
if a later workload fails. This runner intentionally contains no network client.
"""

from __future__ import annotations

import argparse
from collections import defaultdict
from datetime import datetime, timezone
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import random
import re
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time
from typing import Any, Iterable


REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_MANIFEST = REPO_ROOT / "benchmarks" / "engine-matrix.json"
SCORE_RE = re.compile(r"^([^:]+):\s+([0-9]+(?:\.[0-9]+)?)$")


class BenchmarkError(RuntimeError):
    pass


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat(timespec="seconds")


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def atomic_json(path: Path, value: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp")
    with temporary.open("w", encoding="utf-8") as output:
        json.dump(value, output, indent=2, sort_keys=True)
        output.write("\n")
        output.flush()
        os.fsync(output.fileno())
    os.replace(temporary, path)


def resolve_inside(root: Path, relative: str, kind: str) -> Path:
    candidate = (root / relative).resolve()
    try:
        candidate.relative_to(root.resolve())
    except ValueError as error:
        raise BenchmarkError(f"{kind} path escapes repository: {relative}") from error
    return candidate


def command_output(command: list[str], cwd: Path, env: dict[str, str]) -> str | None:
    try:
        completed = subprocess.run(
            command,
            cwd=cwd,
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=15,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    if completed.returncode != 0:
        return None
    return completed.stdout.decode("utf-8", errors="replace").strip()


def git_provenance(identifier: str, path: Path, env: dict[str, str]) -> dict[str, Any]:
    if not path.is_dir():
        return {"id": identifier, "path": str(path), "available": False}
    head = command_output(["git", "-C", str(path), "rev-parse", "HEAD"], path, env)
    branch = command_output(
        ["git", "-C", str(path), "branch", "--show-current"], path, env
    )
    status = command_output(
        ["git", "-C", str(path), "status", "--short", "--untracked-files=all"],
        path,
        env,
    )
    return {
        "id": identifier,
        "path": str(path),
        "available": head is not None,
        "revision": head,
        "branch": branch,
        "dirty": bool(status),
        "status": status.splitlines() if status else [],
    }


def read_text(path: Path) -> str | None:
    try:
        return path.read_text(encoding="utf-8").strip()
    except OSError:
        return None


def thermal_snapshot() -> list[dict[str, Any]]:
    result: list[dict[str, Any]] = []
    thermal_root = Path("/sys/class/thermal")
    for zone in sorted(thermal_root.glob("thermal_zone*")):
        raw = read_text(zone / "temp")
        if raw is None:
            continue
        try:
            celsius = float(raw) / 1000.0
        except ValueError:
            continue
        result.append(
            {
                "zone": zone.name,
                "type": read_text(zone / "type"),
                "celsius": celsius,
            }
        )
    return result


def host_snapshot(cpu_affinity: list[int] | None, env: dict[str, str]) -> dict[str, Any]:
    uname = platform.uname()
    allowed = sorted(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else None
    cpu_details: list[dict[str, Any]] = []
    for cpu in cpu_affinity or []:
        base = Path(f"/sys/devices/system/cpu/cpu{cpu}/cpufreq")
        detail: dict[str, Any] = {"cpu": cpu}
        for key in (
            "scaling_governor",
            "scaling_cur_freq",
            "cpuinfo_min_freq",
            "cpuinfo_max_freq",
        ):
            detail[key] = read_text(base / key)
        cpu_details.append(detail)
    try:
        load_average = list(os.getloadavg())
    except OSError:
        load_average = None
    return {
        "hostname": uname.node,
        "operating_system": uname.system,
        "kernel": uname.release,
        "machine": uname.machine,
        "python": platform.python_version(),
        "logical_cpus": os.cpu_count(),
        "allowed_cpu_affinity": allowed,
        "selected_cpu_affinity": cpu_affinity,
        "selected_cpu_details": cpu_details,
        "load_average": load_average,
        "thermal": thermal_snapshot(),
        "rustc": command_output(["rustc", "--version", "--verbose"], REPO_ROOT, env),
        "cargo": command_output(["cargo", "--version"], REPO_ROOT, env),
    }


def validate_manifest(manifest: dict[str, Any]) -> None:
    if manifest.get("schema_version") != 1:
        raise BenchmarkError("unsupported manifest schema_version")
    suite = manifest.get("suite")
    if not isinstance(suite, dict):
        raise BenchmarkError("manifest has no suite object")
    workloads = suite.get("workloads")
    engines = manifest.get("engines")
    if not isinstance(workloads, list) or not workloads:
        raise BenchmarkError("manifest suite has no workloads")
    if not isinstance(engines, list) or not engines:
        raise BenchmarkError("manifest has no engines")
    for collection, label in ((workloads, "workload"), (engines, "engine")):
        identifiers = [entry.get("id") for entry in collection]
        if any(not isinstance(identifier, str) or not identifier for identifier in identifiers):
            raise BenchmarkError(f"manifest has an invalid {label} id")
        if len(set(identifiers)) != len(identifiers):
            raise BenchmarkError(f"manifest has duplicate {label} ids")
    files = suite.get("files")
    if not isinstance(files, list) or not files:
        raise BenchmarkError("manifest suite has no pinned files")
    file_names = set()
    for entry in files:
        name = entry.get("path")
        digest = entry.get("sha256")
        if not isinstance(name, str) or not name:
            raise BenchmarkError("manifest has an invalid fixture path")
        if not isinstance(digest, str) or not re.fullmatch(r"[0-9a-f]{64}", digest):
            raise BenchmarkError(f"manifest has an invalid sha256 for {name}")
        file_names.add(name)
    required = {suite.get("base"), suite.get("driver")}
    required.update(workload.get("file") for workload in workloads)
    missing = required - file_names
    if missing:
        raise BenchmarkError(f"manifest does not hash fixture files: {sorted(missing)}")


def verify_fixtures(root: Path, suite: dict[str, Any]) -> tuple[Path, list[dict[str, Any]]]:
    fixture_root = resolve_inside(root, suite["fixture_root"], "fixture")
    verified: list[dict[str, Any]] = []
    for entry in suite["files"]:
        path = resolve_inside(fixture_root, entry["path"], "fixture")
        if not path.is_file():
            raise BenchmarkError(
                f"missing pinned fixture {path}; run scripts/fetch-v8-v7.py before benchmarking"
            )
        actual = sha256_file(path)
        if actual != entry["sha256"]:
            raise BenchmarkError(
                f"fixture {path} has sha256 {actual}, expected {entry['sha256']}"
            )
        verified.append(
            {
                "path": entry["path"],
                "role": entry.get("role"),
                "sha256": actual,
                "bytes": path.stat().st_size,
            }
        )
    return fixture_root, verified


def sanitized_environment(run_config: dict[str, Any]) -> tuple[dict[str, str], dict[str, Any]]:
    env = dict(os.environ)
    prefixes = run_config.get("clear_environment_prefixes", [])
    removed: list[str] = []
    for key in list(env):
        if any(key.startswith(prefix) for prefix in prefixes):
            removed.append(key)
            del env[key]
    configured = {str(key): str(value) for key, value in run_config.get("environment", {}).items()}
    env.update(configured)
    return env, {
        "set": configured,
        "clear_prefixes": prefixes,
        "removed_keys": sorted(removed),
    }


def resolve_program(root: Path, program: str) -> Path | None:
    local = (root / program).resolve()
    if local.is_file() and os.access(local, os.X_OK):
        return local
    if "/" in program or program.startswith("."):
        return None
    found = shutil.which(program)
    return Path(found).resolve() if found else None


def run_build(command: list[str], root: Path, env: dict[str, str]) -> dict[str, Any]:
    started = time.perf_counter()
    completed = subprocess.run(
        command,
        cwd=root,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
    )
    elapsed = time.perf_counter() - started
    output = completed.stdout.decode("utf-8", errors="replace")
    result = {
        "command": command,
        "wall_seconds": elapsed,
        "exit_code": completed.returncode,
        "output_tail": output.splitlines()[-40:],
    }
    if completed.returncode != 0:
        raise BenchmarkError(f"build failed ({completed.returncode}): {' '.join(command)}")
    return result


def prepare_engines(
    root: Path,
    configured: list[dict[str, Any]],
    requested: list[str],
    env: dict[str, str],
    no_build: bool,
    allow_version_mismatch: bool,
) -> tuple[list[dict[str, Any]], list[dict[str, Any]], list[dict[str, Any]]]:
    by_id = {entry["id"]: entry for entry in configured}
    unknown = set(requested) - set(by_id)
    if unknown:
        raise BenchmarkError(f"unknown engines: {sorted(unknown)}")
    candidates = [by_id[identifier] for identifier in requested] if requested else configured
    builds: list[dict[str, Any]] = []
    if not no_build:
        seen_builds: set[tuple[str, ...]] = set()
        for entry in candidates:
            command = entry.get("build")
            if not command:
                continue
            key = tuple(command)
            if key not in seen_builds:
                print(f"building {entry['label']} (offline)...", file=sys.stderr)
                builds.append(run_build(command, root, env))
                seen_builds.add(key)

    ready: list[dict[str, Any]] = []
    skipped: list[dict[str, Any]] = []
    for entry in candidates:
        program = resolve_program(root, entry["program"])
        if program is None:
            reason = f"program not found: {entry['program']}"
            if requested or entry.get("required", False):
                raise BenchmarkError(reason)
            skipped.append({"id": entry["id"], "reason": reason})
            continue
        version = command_output(
            [str(program), *entry.get("version_args", [])], root, env
        )
        expected = entry.get("expected_version")
        matches = expected is None or version == expected
        if not matches and not allow_version_mismatch:
            raise BenchmarkError(
                f"{entry['id']} version is {version!r}, expected {expected!r}; update the manifest "
                "or use --allow-version-mismatch for an exploratory report"
            )
        prepared = dict(entry)
        prepared["resolved_program"] = str(program)
        prepared["actual_version"] = version
        prepared["version_matches_manifest"] = matches
        prepared["program_sha256"] = sha256_file(program)
        prepared["program_bytes"] = program.stat().st_size
        artifact = entry.get("artifact")
        if artifact:
            artifact_path = resolve_inside(root, artifact, "artifact")
            prepared["artifact"] = {
                "path": str(artifact_path),
                "available": artifact_path.is_file(),
                "bytes": artifact_path.stat().st_size if artifact_path.is_file() else None,
                "sha256": sha256_file(artifact_path) if artifact_path.is_file() else None,
            }
        prepared["code_metrics"] = {
            "executable_file_bytes": prepared["program_bytes"],
            "generated_jit_code_bytes": None,
            "jit_compile_seconds": None,
            "status": (
                "reported per sample"
                if entry.get("metrics_stderr_prefix")
                else "engine does not expose compatible process metrics"
            ),
        }
        ready.append(prepared)
    if not ready:
        raise BenchmarkError("no benchmark engines are available")
    return ready, skipped, builds


def accepted_production(root: Path, relative: str | None) -> dict[str, Any] | None:
    if relative is None:
        return None
    path = resolve_inside(root, relative, "accepted-production")
    if not path.is_file():
        raise BenchmarkError(f"accepted production file does not exist: {path}")
    data = json.loads(path.read_text(encoding="utf-8"))
    checks: list[dict[str, Any]] = []
    for entry in data.get("installed_artifacts", []):
        artifact = Path(entry["path"]).expanduser()
        actual = sha256_file(artifact) if artifact.is_file() else None
        checks.append(
            {
                "id": entry["id"],
                "path": str(artifact),
                "expected_sha256": entry["sha256"],
                "actual_sha256": actual,
                "matches": actual == entry["sha256"],
            }
        )
    return {"record": data, "artifact_checks": checks, "record_sha256": sha256_file(path)}


def balanced_schedule(
    phase: str,
    rounds: int,
    workloads: list[str],
    engines: list[str],
    seed: int,
) -> list[dict[str, Any]]:
    if rounds <= 0:
        return []
    rng = random.Random(seed)
    base_workloads = workloads[:]
    base_engines = engines[:]
    rng.shuffle(base_workloads)
    rng.shuffle(base_engines)
    entries: list[dict[str, Any]] = []
    order = 0
    for round_index in range(rounds):
        workload_offset = round_index % len(base_workloads)
        workload_order = base_workloads[workload_offset:] + base_workloads[:workload_offset]
        for workload_position, workload in enumerate(workload_order):
            engine_offset = (round_index + workload_position) % len(base_engines)
            engine_order = base_engines[engine_offset:] + base_engines[:engine_offset]
            for engine_position, engine in enumerate(engine_order):
                entries.append(
                    {
                        "phase": phase,
                        "round": round_index,
                        "order": order,
                        "workload": workload,
                        "workload_position": workload_position,
                        "engine": engine,
                        "engine_position": engine_position,
                    }
                )
                order += 1
    return entries


def derive_driver(source: Path, destination: Path) -> str:
    lines = source.read_bytes().splitlines(keepends=True)
    derived = b"".join(line for line in lines if not line.startswith(b"load("))
    destination.write_bytes(derived)
    return sha256_bytes(derived)


def combined_input(base: Path, workload: Path, driver: Path, destination: Path) -> str:
    payload = b"globalThis.print = (...a) => console.log(...a);\n"
    for path in (base, workload, driver):
        payload += path.read_bytes()
        if not payload.endswith(b"\n"):
            payload += b"\n"
    destination.write_bytes(payload)
    return sha256_bytes(payload)


def normalize_rss_bytes(max_rss: int) -> int:
    if sys.platform == "darwin":
        return max_rss
    return max_rss * 1024


def run_measured_process(
    command: list[str], cwd: Path, env: dict[str, str], timeout_seconds: float
) -> dict[str, Any]:
    with tempfile.TemporaryFile() as stdout_file, tempfile.TemporaryFile() as stderr_file:
        started = time.perf_counter()
        process = subprocess.Popen(
            command,
            cwd=cwd,
            env=env,
            stdout=stdout_file,
            stderr=stderr_file,
            start_new_session=True,
        )
        deadline = started + timeout_seconds
        timed_out = False
        status: int | None = None
        usage = None
        while status is None:
            waited_pid, waited_status, waited_usage = os.wait4(process.pid, os.WNOHANG)
            if waited_pid == process.pid:
                status = waited_status
                usage = waited_usage
                break
            if time.perf_counter() >= deadline:
                timed_out = True
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                _, status, usage = os.wait4(process.pid, 0)
                break
            time.sleep(0.01)
        wall_seconds = time.perf_counter() - started
        assert status is not None and usage is not None
        exit_code = os.waitstatus_to_exitcode(status)
        process.returncode = exit_code
        stdout_file.seek(0)
        stderr_file.seek(0)
        stdout = stdout_file.read()
        stderr = stderr_file.read()
    return {
        "exit_code": exit_code,
        "timed_out": timed_out,
        "wall_seconds": wall_seconds,
        "user_cpu_seconds": usage.ru_utime,
        "system_cpu_seconds": usage.ru_stime,
        "cpu_seconds": usage.ru_utime + usage.ru_stime,
        "peak_rss_bytes": normalize_rss_bytes(usage.ru_maxrss),
        "minor_page_faults": usage.ru_minflt,
        "major_page_faults": usage.ru_majflt,
        "voluntary_context_switches": usage.ru_nvcsw,
        "involuntary_context_switches": usage.ru_nivcsw,
        "stdout": stdout.decode("utf-8", errors="replace"),
        "stderr": stderr.decode("utf-8", errors="replace"),
    }


def parse_score(stdout: str, label: str) -> float:
    for line in stdout.splitlines():
        match = SCORE_RE.match(line.strip())
        if match and match.group(1) == label:
            return float(match.group(2))
    raise BenchmarkError(f"benchmark output did not contain {label!r} score")


def parse_engine_metrics(stderr: str, prefix: str | None) -> dict[str, Any] | None:
    if prefix is None:
        return None
    matches = [line[len(prefix) :] for line in stderr.splitlines() if line.startswith(prefix)]
    if len(matches) != 1:
        raise BenchmarkError(
            f"expected exactly one engine metrics line with prefix {prefix!r}, found {len(matches)}"
        )
    try:
        metrics = json.loads(matches[0])
    except json.JSONDecodeError as error:
        raise BenchmarkError(f"invalid engine metrics JSON: {error}") from error
    if metrics.get("schema_version") != 1:
        raise BenchmarkError("unsupported engine metrics schema_version")
    histogram = metrics.get("gc_pause_histogram")
    if histogram is not None:
        bounds = histogram.get("upper_bounds")
        counts = histogram.get("counts")
        if (
            histogram.get("unit") != "nanoseconds"
            or not isinstance(bounds, list)
            or not isinstance(counts, list)
            or len(bounds) != len(counts)
            or not bounds
            or bounds[-1] is not None
            or any(not isinstance(count, int) or count < 0 for count in counts)
            or sum(counts) != metrics.get("gc_collections")
        ):
            raise BenchmarkError("invalid GC pause histogram in engine metrics")
    managed = metrics.get("managed_memory")
    if managed is not None:
        if managed.get("schema_version") != 1 or not isinstance(managed.get("complete"), bool):
            raise BenchmarkError("invalid managed-memory record in engine metrics")
        if not isinstance(managed.get("agent_id"), int) or not isinstance(
            managed.get("heap_id"), int
        ):
            raise BenchmarkError("managed-memory record lacks Agent/heap identity")
        if managed.get("safepoint") is None:
            if managed.get("complete"):
                raise BenchmarkError("managed-memory record cannot be complete before a safepoint")
            for name in ("managed_requested_bytes", "managed_external_bytes"):
                measurement = managed.get(name)
                if (
                    not isinstance(measurement, dict)
                    or measurement.get("bytes") is not None
                    or measurement.get("quality") != "unavailable"
                    or not isinstance(measurement.get("reason"), str)
                    or not measurement["reason"]
                ):
                    raise BenchmarkError(f"invalid unavailable {name} record")
        else:
            if managed.get("safepoint") != "post_gc" or not isinstance(
                managed.get("categories"), dict
            ):
                raise BenchmarkError("invalid managed-memory safepoint record")
            for name in ("managed_requested_bytes", "managed_external_bytes"):
                measurement = managed.get(name)
                if (
                    not isinstance(measurement, dict)
                    or not isinstance(measurement.get("bytes"), int)
                    or measurement["bytes"] < 0
                    or measurement.get("quality") not in ("exact", "lower_bound")
                    or (
                        measurement.get("quality") == "lower_bound"
                        and (
                            not isinstance(measurement.get("reason"), str)
                            or not measurement["reason"]
                        )
                    )
                ):
                    raise BenchmarkError(f"invalid {name} in managed-memory record")
            for name, category in managed["categories"].items():
                if not isinstance(name, str) or not isinstance(category, dict):
                    raise BenchmarkError("invalid managed-memory category")
                quality = category.get("quality")
                byte_count = category.get("bytes")
                if quality == "unavailable":
                    if (
                        byte_count is not None
                        or not isinstance(category.get("reason"), str)
                        or not category["reason"]
                    ):
                        raise BenchmarkError(
                            "unavailable managed-memory category lacks a reason or has bytes"
                        )
                elif quality in ("exact", "lower_bound"):
                    if not isinstance(byte_count, int) or byte_count < 0:
                        raise BenchmarkError("invalid managed-memory category byte count")
                    if quality == "lower_bound" and (
                        not isinstance(category.get("reason"), str) or not category["reason"]
                    ):
                        raise BenchmarkError("lower-bound managed-memory category lacks a reason")
                else:
                    raise BenchmarkError("invalid managed-memory category quality")
    return metrics


def stable_seed(seed: int, label: str) -> int:
    digest = hashlib.sha256(f"{seed}:{label}".encode()).digest()
    return int.from_bytes(digest[:8], "little")


def percentile(sorted_values: list[float], quantile: float) -> float:
    if len(sorted_values) == 1:
        return sorted_values[0]
    position = quantile * (len(sorted_values) - 1)
    lower = math.floor(position)
    upper = math.ceil(position)
    if lower == upper:
        return sorted_values[lower]
    fraction = position - lower
    return sorted_values[lower] * (1.0 - fraction) + sorted_values[upper] * fraction


def summary_stats(
    values: Iterable[float], confidence: float, resamples: int, seed: int
) -> dict[str, Any]:
    samples = [float(value) for value in values]
    if not samples:
        raise BenchmarkError("cannot summarize an empty sample")
    median = statistics.median(samples)
    mean = statistics.fmean(samples)
    result: dict[str, Any] = {
        "count": len(samples),
        "median": median,
        "mean": mean,
        "minimum": min(samples),
        "maximum": max(samples),
        "standard_deviation": statistics.stdev(samples) if len(samples) > 1 else None,
        "coefficient_of_variation": (
            statistics.stdev(samples) / mean if len(samples) > 1 and mean != 0 else None
        ),
        "median_confidence_interval": None,
    }
    if len(samples) > 1 and resamples > 0:
        rng = random.Random(seed)
        bootstrapped = []
        for _ in range(resamples):
            bootstrapped.append(
                statistics.median(rng.choice(samples) for _ in range(len(samples)))
            )
        bootstrapped.sort()
        alpha = (1.0 - confidence) / 2.0
        result["median_confidence_interval"] = [
            percentile(bootstrapped, alpha),
            percentile(bootstrapped, 1.0 - alpha),
        ]
    return result


def geometric_mean(values: Iterable[float]) -> float:
    values_list = list(values)
    if not values_list or any(value <= 0 for value in values_list):
        raise BenchmarkError("geometric mean requires positive values")
    return math.exp(statistics.fmean(math.log(value) for value in values_list))


def summarize(
    samples: list[dict[str, Any]],
    engines: list[str],
    workloads: list[str],
    reference: str,
    confidence: float,
    resamples: int,
    seed: int,
) -> dict[str, Any]:
    measured = [sample for sample in samples if sample["phase"] == "measure"]
    grouped: dict[tuple[str, str], list[dict[str, Any]]] = defaultdict(list)
    by_round: dict[tuple[str, int], dict[str, float]] = defaultdict(dict)
    for sample in measured:
        grouped[(sample["engine"], sample["workload"])].append(sample)
        by_round[(sample["engine"], sample["round"])][sample["workload"]] = sample["score"]

    engine_results: dict[str, Any] = {}
    composite_by_engine: dict[str, dict[int, float]] = defaultdict(dict)
    for engine in engines:
        workload_results: dict[str, Any] = {}
        for workload in workloads:
            entries = grouped[(engine, workload)]
            result = {
                metric: summary_stats(
                    (entry[metric] for entry in entries),
                    confidence,
                    resamples,
                    stable_seed(seed, f"{engine}:{workload}:{metric}"),
                )
                for metric in ("score", "wall_seconds", "cpu_seconds", "peak_rss_bytes")
            }
            engine_metric_entries = [
                entry["engine_metrics"] for entry in entries if entry.get("engine_metrics")
            ]
            if engine_metric_entries:
                summarized_engine_metrics = {
                    metric: summary_stats(
                        (entry[metric] for entry in engine_metric_entries),
                        confidence,
                        resamples,
                        stable_seed(seed, f"{engine}:{workload}:engine-metrics:{metric}"),
                    )
                    for metric in (
                        "jit_compile_attempts",
                        "jit_compile_successes",
                        "jit_compile_failures",
                        "jit_compile_seconds",
                        "jit_generated_code_bytes",
                        "jit_largest_code_bytes",
                        "gc_collections",
                        "gc_pause_seconds",
                        "gc_max_pause_seconds",
                        "gc_objects_seen",
                        "gc_objects_reclaimed",
                        "gc_peak_objects_before",
                        "gc_last_objects_after",
                        "gc_scopes_seen",
                        "gc_scopes_reclaimed",
                        "gc_peak_scopes_before",
                        "gc_last_scopes_after",
                    )
                }
                histograms = [entry["gc_pause_histogram"] for entry in engine_metric_entries]
                histogram_unit = histograms[0]["unit"]
                histogram_bounds = histograms[0]["upper_bounds"]
                if any(
                    histogram["unit"] != histogram_unit
                    or histogram["upper_bounds"] != histogram_bounds
                    or len(histogram["counts"]) != len(histogram_bounds)
                    for histogram in histograms
                ):
                    raise BenchmarkError("incompatible GC pause histograms in engine metrics")
                summarized_engine_metrics["gc_pause_histogram_aggregate"] = {
                    "unit": histogram_unit,
                    "upper_bounds": histogram_bounds,
                    "counts": [
                        sum(histogram["counts"][index] for histogram in histograms)
                        for index in range(len(histogram_bounds))
                    ],
                    "sample_processes": len(histograms),
                }
                managed_records = [
                    entry["managed_memory"]
                    for entry in engine_metric_entries
                    if entry.get("managed_memory") is not None
                ]
                if managed_records:
                    if len(managed_records) != len(engine_metric_entries):
                        raise BenchmarkError("managed-memory records missing from some samples")
                    category_names = set(managed_records[0]["categories"])
                    if any(
                        set(record["categories"]) != category_names for record in managed_records
                    ):
                        raise BenchmarkError("incompatible managed-memory categories")
                    managed_summary: dict[str, Any] = {
                        "schema_version": managed_records[0]["schema_version"],
                        "complete": all(record["complete"] for record in managed_records),
                    }
                    for name in ("managed_requested_bytes", "managed_external_bytes"):
                        qualities = sorted({record[name]["quality"] for record in managed_records})
                        managed_summary[name] = {
                            "quality": qualities[0] if len(qualities) == 1 else qualities,
                            "bytes": summary_stats(
                                (record[name]["bytes"] for record in managed_records),
                                confidence,
                                resamples,
                                stable_seed(seed, f"{engine}:{workload}:engine-metrics:{name}"),
                            ),
                        }
                    category_summary = {}
                    for name in sorted(category_names):
                        categories = [record["categories"][name] for record in managed_records]
                        qualities = sorted({category["quality"] for category in categories})
                        available = [
                            category["bytes"]
                            for category in categories
                            if category["bytes"] is not None
                        ]
                        category_summary[name] = {
                            "quality": qualities[0] if len(qualities) == 1 else qualities,
                            "bytes": (
                                summary_stats(
                                    available,
                                    confidence,
                                    resamples,
                                    stable_seed(
                                        seed,
                                        f"{engine}:{workload}:engine-metrics:managed:{name}",
                                    ),
                                )
                                if available
                                else None
                            ),
                        }
                    managed_summary["categories"] = category_summary
                    summarized_engine_metrics["managed_memory"] = managed_summary
                result["engine_metrics"] = summarized_engine_metrics
            workload_results[workload] = result
        rounds = sorted(round_index for (candidate, round_index) in by_round if candidate == engine)
        for round_index in rounds:
            values = by_round[(engine, round_index)]
            if all(workload in values for workload in workloads):
                composite_by_engine[engine][round_index] = geometric_mean(
                    values[workload] for workload in workloads
                )
        composites = list(composite_by_engine[engine].values())
        engine_results[engine] = {
            "workloads": workload_results,
            "composite_score": summary_stats(
                composites,
                confidence,
                resamples,
                stable_seed(seed, f"{engine}:composite"),
            ),
            "composite_by_round": composite_by_engine[engine],
        }

    comparisons: dict[str, Any] = {"reference_engine": reference, "engines": {}}
    reference_composite = composite_by_engine[reference]
    for engine in engines:
        if engine == reference:
            continue
        workload_comparisons: dict[str, Any] = {}
        for workload in workloads:
            reference_by_round = {
                entry["round"]: entry["score"] for entry in grouped[(reference, workload)]
            }
            candidate_by_round = {
                entry["round"]: entry["score"] for entry in grouped[(engine, workload)]
            }
            common = sorted(set(reference_by_round) & set(candidate_by_round))
            ratios = [candidate_by_round[index] / reference_by_round[index] for index in common]
            workload_comparisons[workload] = summary_stats(
                ratios,
                confidence,
                resamples,
                stable_seed(seed, f"compare:{reference}:{engine}:{workload}"),
            )
        common_composite = sorted(set(reference_composite) & set(composite_by_engine[engine]))
        composite_ratios = [
            composite_by_engine[engine][index] / reference_composite[index]
            for index in common_composite
        ]
        comparisons["engines"][engine] = {
            "candidate_over_reference_score_ratio": workload_comparisons,
            "composite_candidate_over_reference_score_ratio": summary_stats(
                composite_ratios,
                confidence,
                resamples,
                stable_seed(seed, f"compare:{reference}:{engine}:composite"),
            ),
        }
    return {"engines": engine_results, "comparisons": comparisons}


def parse_cpu(value: str | None, configured: list[int]) -> list[int] | None:
    if value is None:
        return configured
    if value.lower() == "none":
        return None
    try:
        cpus = [int(part) for part in value.split(",")]
    except ValueError as error:
        raise BenchmarkError("--cpu must be a comma-separated CPU list or 'none'") from error
    if not cpus or any(cpu < 0 for cpu in cpus):
        raise BenchmarkError("--cpu must contain non-negative CPU ids")
    return cpus


def build_command(
    engine: dict[str, Any],
    workload: dict[str, Any],
    fixture_root: Path,
    base_file: str,
    driver: Path,
    combined: Path,
    affinity: list[int] | None,
) -> list[str]:
    command: list[str] = []
    if affinity is not None:
        taskset = shutil.which("taskset")
        if taskset is None:
            raise BenchmarkError("CPU affinity requested but taskset is unavailable")
        command.extend([taskset, "-c", ",".join(str(cpu) for cpu in affinity)])
    command.extend([engine["resolved_program"], *engine.get("args", [])])
    if engine["input_mode"] == "combined":
        command.append(str(combined))
    elif engine["input_mode"] == "separate":
        command.extend(
            [
                str(fixture_root / base_file),
                str(fixture_root / workload["file"]),
                str(driver),
            ]
        )
    else:
        raise BenchmarkError(f"unknown input_mode for {engine['id']}: {engine['input_mode']}")
    return command


def default_output(root: Path) -> Path:
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S%fZ")
    return root / "benchmark-results" / f"engine-matrix-{stamp}.json"


def argument_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    parser.add_argument("--repo-root", type=Path, default=REPO_ROOT)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--engine", action="append", default=[])
    parser.add_argument("--workload", action="append", default=[])
    parser.add_argument("--samples", type=int)
    parser.add_argument("--warmups", type=int)
    parser.add_argument("--cpu", help="comma-separated CPU ids, or 'none'")
    parser.add_argument("--seed", type=int)
    parser.add_argument("--no-build", action="store_true")
    parser.add_argument("--allow-version-mismatch", action="store_true")
    parser.add_argument("--list", action="store_true", help="list configured engines/workloads")
    return parser


def run(args: argparse.Namespace) -> int:
    root = args.repo_root.resolve()
    manifest_path = args.manifest.resolve()
    manifest_bytes = manifest_path.read_bytes()
    manifest = json.loads(manifest_bytes)
    validate_manifest(manifest)
    suite = manifest["suite"]
    run_config = manifest["run"]
    workload_by_id = {entry["id"]: entry for entry in suite["workloads"]}
    engine_ids = {entry["id"] for entry in manifest["engines"]}
    if args.list:
        print("engines:")
        for engine in manifest["engines"]:
            print(f"  {engine['id']}: {engine['label']}")
        print("workloads:")
        for workload in suite["workloads"]:
            print(f"  {workload['id']}: {workload['label']}")
        return 0
    unknown_workloads = set(args.workload) - set(workload_by_id)
    if unknown_workloads:
        raise BenchmarkError(f"unknown workloads: {sorted(unknown_workloads)}")
    if set(args.engine) - engine_ids:
        raise BenchmarkError(f"unknown engines: {sorted(set(args.engine) - engine_ids)}")
    workloads = (
        [workload_by_id[identifier] for identifier in args.workload]
        if args.workload
        else suite["workloads"]
    )
    sample_rounds = args.samples if args.samples is not None else run_config["sample_rounds"]
    warmup_rounds = args.warmups if args.warmups is not None else run_config["warmup_rounds"]
    if sample_rounds <= 0 or warmup_rounds < 0:
        raise BenchmarkError("samples must be positive and warmups must be non-negative")
    seed = args.seed if args.seed is not None else run_config["schedule_seed"]
    affinity = parse_cpu(args.cpu, run_config.get("cpu_affinity", []))
    if affinity is not None and hasattr(os, "sched_getaffinity"):
        unavailable = set(affinity) - os.sched_getaffinity(0)
        if unavailable:
            raise BenchmarkError(f"requested CPUs are outside this process affinity: {sorted(unavailable)}")

    output_path = args.output.resolve() if args.output else default_output(root)
    env, environment_policy = sanitized_environment(run_config)
    report: dict[str, Any] = {
        "schema_version": 1,
        "status": "initializing",
        "started_at": utc_now(),
        "completed_at": None,
        "output_path": str(output_path),
        "manifest": {
            "path": str(manifest_path),
            "sha256": sha256_bytes(manifest_bytes),
            "schema_version": manifest["schema_version"],
        },
        "configuration": {
            "suite": suite["id"],
            "engines_requested": args.engine or "all available",
            "workloads": [entry["id"] for entry in workloads],
            "warmup_rounds": warmup_rounds,
            "sample_rounds": sample_rounds,
            "timeout_seconds": run_config["timeout_seconds"],
            "cpu_affinity": affinity,
            "schedule_seed": seed,
            "confidence_level": run_config["confidence_level"],
            "bootstrap_resamples": run_config["bootstrap_resamples"],
            "no_build": args.no_build,
            "allow_version_mismatch": args.allow_version_mismatch,
            "cli_overrides_manifest": any(
                value
                for value in (
                    args.engine,
                    args.workload,
                    args.samples is not None,
                    args.warmups is not None,
                    args.cpu is not None,
                    args.seed is not None,
                    args.no_build,
                    args.allow_version_mismatch,
                )
            ),
            "environment_policy": environment_policy,
        },
        "host_start": host_snapshot(affinity, env),
        "fixtures": None,
        "provenance": {},
        "engines": [],
        "skipped_engines": [],
        "builds": [],
        "schedule": [],
        "derived_inputs": {},
        "samples": [],
        "summary": None,
        "error": None,
    }

    def checkpoint() -> None:
        atomic_json(output_path, report)

    checkpoint()
    try:
        fixture_root, verified = verify_fixtures(root, suite)
        report["fixtures"] = {
            "root": str(fixture_root),
            "source": suite["source"],
            "files": verified,
        }
        provenance_config = manifest.get("provenance", {})
        report["provenance"] = {
            "repositories": [
                git_provenance(entry["id"], (root / entry["path"]).resolve(), env)
                for entry in provenance_config.get("repositories", [])
            ],
            "accepted_production": accepted_production(
                root, provenance_config.get("accepted_production")
            ),
        }
        engines, skipped, builds = prepare_engines(
            root,
            manifest["engines"],
            args.engine,
            env,
            args.no_build,
            args.allow_version_mismatch,
        )
        report["engines"] = engines
        report["skipped_engines"] = skipped
        report["builds"] = builds
        selected_engine_ids = [entry["id"] for entry in engines]
        reference = manifest.get("reference_engine")
        if reference not in selected_engine_ids:
            reference = selected_engine_ids[0]
        warmup_schedule = balanced_schedule(
            "warmup",
            warmup_rounds,
            [entry["id"] for entry in workloads],
            selected_engine_ids,
            seed ^ 0xA5A5A5A5,
        )
        measured_schedule = balanced_schedule(
            "measure",
            sample_rounds,
            [entry["id"] for entry in workloads],
            selected_engine_ids,
            seed,
        )
        schedule = warmup_schedule + measured_schedule
        for index, entry in enumerate(schedule):
            entry["global_order"] = index
        report["schedule"] = schedule
        report["status"] = "running"
        checkpoint()

        engines_by_id = {entry["id"]: entry for entry in engines}
        with tempfile.TemporaryDirectory(prefix="lumen-bench-matrix-") as temporary_name:
            temporary = Path(temporary_name)
            driver = temporary / "driver.js"
            driver_hash = derive_driver(fixture_root / suite["driver"], driver)
            combined_files: dict[str, Path] = {}
            combined_hashes: dict[str, str] = {}
            for workload in workloads:
                path = temporary / f"combined-{workload['id']}.js"
                combined_hashes[workload["id"]] = combined_input(
                    fixture_root / suite["base"],
                    fixture_root / workload["file"],
                    driver,
                    path,
                )
                combined_files[workload["id"]] = path
            report["derived_inputs"] = {
                "driver_sha256": driver_hash,
                "combined_sha256": combined_hashes,
            }
            checkpoint()
            for entry in schedule:
                workload = workload_by_id[entry["workload"]]
                engine = engines_by_id[entry["engine"]]
                command = build_command(
                    engine,
                    workload,
                    fixture_root,
                    suite["base"],
                    driver,
                    combined_files[workload["id"]],
                    affinity,
                )
                print(
                    f"[{entry['global_order'] + 1}/{len(schedule)}] {entry['phase']} "
                    f"round={entry['round'] + 1} {engine['id']} {workload['id']}",
                    file=sys.stderr,
                    flush=True,
                )
                sample_started_at = utc_now()
                sample_env = dict(env)
                engine_environment = {
                    str(key): str(value) for key, value in engine.get("environment", {}).items()
                }
                sample_env.update(engine_environment)
                measured = run_measured_process(
                    command, root, sample_env, float(run_config["timeout_seconds"])
                )
                raw_stdout = measured.pop("stdout")
                raw_stderr = measured.pop("stderr")
                sample = {
                    **entry,
                    "started_at": sample_started_at,
                    "completed_at": utc_now(),
                    "command": command,
                    "engine_environment": engine_environment,
                    **measured,
                    "stdout_sha256": sha256_bytes(raw_stdout.encode()),
                    "stdout_lines": raw_stdout.splitlines(),
                    "stderr_sha256": sha256_bytes(raw_stderr.encode()),
                    "stderr_tail": raw_stderr.splitlines()[-40:],
                }
                report["samples"].append(sample)
                checkpoint()
                if measured["timed_out"]:
                    raise BenchmarkError(
                        f"{engine['id']} {workload['id']} exceeded "
                        f"{run_config['timeout_seconds']} seconds"
                    )
                if measured["exit_code"] != 0:
                    raise BenchmarkError(
                        f"{engine['id']} {workload['id']} exited {measured['exit_code']}"
                    )
                sample["score"] = parse_score(raw_stdout, workload["label"])
                sample["engine_metrics"] = parse_engine_metrics(
                    raw_stderr, engine.get("metrics_stderr_prefix")
                )
                checkpoint()

        report["summary"] = summarize(
            report["samples"],
            selected_engine_ids,
            [entry["id"] for entry in workloads],
            reference,
            float(run_config["confidence_level"]),
            int(run_config["bootstrap_resamples"]),
            seed,
        )
        report["host_end"] = host_snapshot(affinity, env)
        report["status"] = "complete"
        report["completed_at"] = utc_now()
        checkpoint()
        print(f"report: {output_path}", file=sys.stderr)
        return 0
    except Exception as error:
        report["status"] = "failed"
        report["completed_at"] = utc_now()
        report["error"] = {"type": type(error).__name__, "message": str(error)}
        report["host_end"] = host_snapshot(affinity, env)
        checkpoint()
        print(f"error: {error}", file=sys.stderr)
        print(f"partial report: {output_path}", file=sys.stderr)
        return 1


def main() -> int:
    parser = argument_parser()
    args = parser.parse_args()
    try:
        return run(args)
    except (BenchmarkError, OSError, ValueError, json.JSONDecodeError) as error:
        parser.error(str(error))
    return 2


if __name__ == "__main__":
    raise SystemExit(main())

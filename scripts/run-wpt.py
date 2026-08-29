#!/usr/bin/env python3
"""Run Lumen's focused, shell-compatible Web Platform Test manifest."""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import os
from pathlib import Path
import subprocess
import sys
import time


ROOT = Path(__file__).resolve().parent.parent
HARNESS_PREFIX = "__LUMEN_WPT_H__"
TEST_PREFIX = "__LUMEN_WPT_T__"


def manifest(path: Path) -> list[str]:
    tests: list[str] = []
    for raw in path.read_text(encoding="utf-8").splitlines():
        line = raw.strip()
        if line and not line.startswith("#"):
            tests.append(line)
    return tests


def run_one(binary: Path, shell: Path, wpt: Path, test: str, timeout: int) -> dict:
    started = time.monotonic()
    try:
        completed = subprocess.run(
            [str(binary), str(shell), str(wpt), test],
            cwd=ROOT,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            timeout=timeout,
            check=False,
        )
    except subprocess.TimeoutExpired as error:
        return {
            "file": test,
            "harness": {"status": 2, "message": f"process timeout after {timeout}s"},
            "tests": [],
            "stderr": error.stderr or "",
            "elapsed_ms": round((time.monotonic() - started) * 1000),
        }

    payload = None
    tests = []
    noise = []
    for line in completed.stdout.splitlines():
        if line.startswith(HARNESS_PREFIX):
            try:
                harness = json.loads(line[len(HARNESS_PREFIX) :])
                payload = {
                    "file": harness.pop("file"),
                    "harness": harness,
                    "tests": tests,
                }
            except json.JSONDecodeError as error:
                noise.append(f"malformed harness record: {error}")
        elif line.startswith(TEST_PREFIX):
            try:
                tests.append(json.loads(line[len(TEST_PREFIX) :]))
            except json.JSONDecodeError as error:
                noise.append(f"malformed subtest record: {error}")
        elif line:
            noise.append(line)
    if payload is None:
        payload = {
            "file": test,
            "harness": {
                "status": 1,
                "message": f"runner exited {completed.returncode} without a WPT result",
            },
            "tests": [],
        }
    payload["stderr"] = completed.stderr
    payload["stdout"] = "\n".join(noise)
    payload["elapsed_ms"] = round((time.monotonic() - started) * 1000)
    return payload


def failed(result: dict) -> bool:
    return result["harness"]["status"] != 0 or any(
        test["status"] not in (0, 4) for test in result["tests"]
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("tests", nargs="*", help="WPT paths; defaults to wpt-focus.txt")
    parser.add_argument("--manifest", type=Path, default=ROOT / "wpt-focus.txt")
    parser.add_argument("--no-build", action="store_true")
    parser.add_argument("--timeout", type=int, default=60)
    args = parser.parse_args()

    wpt = Path(os.environ.get("WPT", ROOT / "wpt")).resolve()
    if not (wpt / "resources" / "testharness.js").is_file():
        subprocess.run([str(ROOT / "scripts" / "wpt-clone.sh")], cwd=ROOT, check=True)
    tests = args.tests or manifest(args.manifest)
    if not tests:
        parser.error("no tests selected")

    binary = Path(os.environ.get("LUMEN_WPT_BIN", ROOT / "target/release/lumen-cli"))
    if not args.no_build and "LUMEN_WPT_BIN" not in os.environ:
        subprocess.run(
            ["cargo", "build", "--release", "-q", "-p", "lumen-cli"], cwd=ROOT, check=True
        )
    if not binary.is_file():
        parser.error(f"runtime not found: {binary}")

    jobs = max(1, min(int(os.environ.get("LUMEN_WPT_JOBS", os.cpu_count() or 1)), 16))
    with concurrent.futures.ThreadPoolExecutor(max_workers=jobs) as pool:
        futures = {
            pool.submit(run_one, binary, ROOT / "scripts/wpt-shell.js", wpt, test, args.timeout): test
            for test in tests
        }
        results = []
        for future in concurrent.futures.as_completed(futures):
            result = future.result()
            results.append(result)
            mark = "FAIL" if failed(result) else "PASS"
            passed = sum(test["status"] == 0 for test in result["tests"])
            print(f"{mark:4} {result['file']} ({passed}/{len(result['tests'])})")

    results.sort(key=lambda result: result["file"])
    revision = subprocess.check_output(["git", "-C", str(wpt), "rev-parse", "HEAD"], text=True).strip()
    report = {
        "wpt_revision": revision,
        "files": len(results),
        "subtests": sum(len(result["tests"]) for result in results),
        "passed": sum(test["status"] == 0 for result in results for test in result["tests"]),
        "failed": sum(test["status"] not in (0, 4) for result in results for test in result["tests"])
        + sum(result["harness"]["status"] != 0 for result in results),
        "results": results,
    }
    report_dir = ROOT / "wpt-report"
    report_dir.mkdir(exist_ok=True)
    (report_dir / "summary.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")

    for result in results:
        if not failed(result):
            continue
        print(f"\n=== {result['file']} ===", file=sys.stderr)
        if result["harness"]["status"]:
            print(f"HARNESS: {result['harness']['message']}", file=sys.stderr)
        for test in result["tests"]:
            if test["status"] not in (0, 4):
                print(f"[{test['status']}] {test['name']}: {test['message']}", file=sys.stderr)
        if result.get("stderr"):
            print(result["stderr"].rstrip(), file=sys.stderr)

    print(
        f"\nWPT {revision}: {report['passed']}/{report['subtests']} subtests passed "
        f"across {report['files']} files; {report['failed']} failures"
    )
    print("wrote wpt-report/summary.json")
    return 1 if report["failed"] else 0


if __name__ == "__main__":
    raise SystemExit(main())

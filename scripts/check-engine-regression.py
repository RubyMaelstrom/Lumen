#!/usr/bin/env python3
"""Check a completed engine-matrix report against a locked Phase 0 policy.

This is deliberately a policy checker rather than a benchmark runner.  It never
recomputes scores from individual samples: the matrix runner owns measurement and
bootstrap statistics, while this tool applies the reviewed thresholds to the
derived medians and confidence intervals in a completed report.
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any


class RegressionError(ValueError):
    """The report or policy is malformed, or a regression is significant."""


def load_json(path: Path) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise RegressionError(f"cannot read {path}: {error}") from error
    if not isinstance(value, dict):
        raise RegressionError(f"{path} must contain a JSON object")
    return value


def _number(value: Any, label: str) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise RegressionError(f"{label} must be a number")
    return float(value)


def validate_policy(policy: dict[str, Any]) -> None:
    if policy.get("schema_version") != 1:
        raise RegressionError("unsupported policy schema_version")
    if not isinstance(policy.get("manifest_sha256"), str):
        raise RegressionError("policy has no manifest_sha256")
    if not isinstance(policy.get("engine"), str) or not policy["engine"]:
        raise RegressionError("policy has no candidate engine")
    components = policy.get("components")
    if not isinstance(components, dict) or not components:
        raise RegressionError("policy has no components")
    for component, entry in components.items():
        if not isinstance(component, str) or not component:
            raise RegressionError("policy has an invalid component name")
        if not isinstance(entry, dict):
            raise RegressionError(f"policy component {component} is not an object")
        baseline = _number(entry.get("baseline_score"), f"{component}.baseline_score")
        ratio = _number(entry.get("minimum_score_ratio"), f"{component}.minimum_score_ratio")
        if baseline <= 0:
            raise RegressionError(f"{component}.baseline_score must be positive")
        if not 0 < ratio <= 1:
            raise RegressionError(f"{component}.minimum_score_ratio must be in (0, 1]")
        if not isinstance(entry.get("reason"), str) or not entry["reason"]:
            raise RegressionError(f"{component}.reason is required")


def _score(entry: Any, component: str) -> tuple[float, tuple[float, float] | None]:
    if not isinstance(entry, dict):
        raise RegressionError(f"report has no score for {component}")
    median = _number(entry.get("median"), f"report {component}.median")
    interval = entry.get("median_confidence_interval")
    if not isinstance(interval, list) or len(interval) != 2:
        raise RegressionError(f"report {component} has no median confidence interval")
    low = _number(interval[0], f"report {component}.median_confidence_interval[0]")
    high = _number(interval[1], f"report {component}.median_confidence_interval[1]")
    if not 0 <= low <= high:
        raise RegressionError(f"report {component} has an invalid confidence interval")
    return median, (low, high)


def check(report: dict[str, Any], policy: dict[str, Any]) -> list[dict[str, Any]]:
    if report.get("schema_version") != 1 or report.get("status") != "complete":
        raise RegressionError("report must be a complete engine-matrix schema 1 report")
    manifest = report.get("manifest")
    if not isinstance(manifest, dict) or manifest.get("sha256") != policy["manifest_sha256"]:
        raise RegressionError("report manifest does not match the locked Phase 0 policy")
    engine_id = policy["engine"]
    engines = report.get("summary", {}).get("engines", {})
    engine = engines.get(engine_id) if isinstance(engines, dict) else None
    if not isinstance(engine, dict):
        raise RegressionError(f"report has no summary for {engine_id}")

    workload_entries = engine.get("workloads", {})
    if not isinstance(workload_entries, dict):
        raise RegressionError(f"report has no workload summary for {engine_id}")
    results: list[dict[str, Any]] = []
    for component, threshold in policy["components"].items():
        if component == "composite":
            score_entry = engine.get("composite_score")
        else:
            workload = workload_entries.get(component)
            score_entry = workload.get("score") if isinstance(workload, dict) else None
        median, interval = _score(score_entry, component)
        baseline = float(threshold["baseline_score"])
        minimum_ratio = float(threshold["minimum_score_ratio"])
        floor = baseline * minimum_ratio
        # A point estimate below the floor is inconclusive until the complete
        # bootstrap interval is below it.  This prevents a noisy run from
        # becoming a false release blocker while still failing clear regressions.
        status = "pass" if median >= floor else "inconclusive"
        if interval[1] < floor:
            status = "regressed"
        results.append(
            {
                "component": component,
                "median": median,
                "baseline": baseline,
                "minimum_score_ratio": minimum_ratio,
                "floor": floor,
                "confidence_interval": list(interval),
                "status": status,
            }
        )
    return results


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("report", type=Path)
    parser.add_argument("policy", type=Path)
    args = parser.parse_args()
    try:
        policy = load_json(args.policy)
        validate_policy(policy)
        results = check(load_json(args.report), policy)
    except RegressionError as error:
        parser.error(str(error))
    for result in results:
        print(
            f"{result['component']}: {result['status']} "
            f"median={result['median']:.3f} floor={result['floor']:.3f} "
            f"CI=[{result['confidence_interval'][0]:.3f}, "
            f"{result['confidence_interval'][1]:.3f}]"
        )
    if any(result["status"] == "regressed" for result in results):
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

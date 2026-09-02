#!/usr/bin/env python3
"""Print a compact human-readable summary of an engine-matrix report.

The benchmark runner remains the source of truth and owns schema validation. This tool is
deliberately read-only: it consumes a completed report and formats the already-derived medians and
confidence intervals without recomputing statistics or silently accepting an incomplete run.
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any


class ReportError(ValueError):
    """The supplied report is not a completed engine-matrix report."""


def load_report(path: Path) -> dict[str, Any]:
    try:
        report = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        raise ReportError(f"cannot read report {path}: {error}") from error
    if not isinstance(report, dict) or report.get("schema_version") != 1:
        raise ReportError("unsupported or malformed engine-matrix report schema")
    if report.get("status") != "complete":
        raise ReportError(f"report status is {report.get('status', '<missing>')!r}, not 'complete'")
    summary = report.get("summary")
    if not isinstance(summary, dict) or not isinstance(summary.get("engines"), dict):
        raise ReportError("completed report has no derived engine summary")
    return report


def _stats_text(stats: Any) -> str:
    if not isinstance(stats, dict) or not isinstance(stats.get("median"), (int, float)):
        return "—"
    median = float(stats["median"])
    interval = stats.get("median_confidence_interval")
    if isinstance(interval, list) and len(interval) == 2 and all(
        isinstance(value, (int, float)) for value in interval
    ):
        return f"{median:.3f} [{float(interval[0]):.3f}, {float(interval[1]):.3f}]"
    return f"{median:.3f}"


def render(report: dict[str, Any]) -> str:
    summary = report["summary"]
    comparisons = summary.get("comparisons", {})
    reference = comparisons.get("reference_engine", "<unknown>")
    engines = summary["engines"]
    workload_names = sorted(
        {
            workload
            for engine in engines.values()
            if isinstance(engine, dict)
            for workload in engine.get("workloads", {})
        }
    )
    lines = [
        f"status: {report.get('status')}",
        f"reference engine: {reference}",
        "",
        "| Engine | Composite median [CI] | " + " | ".join(workload_names) + " |",
        "|---|---:|" + "---:|" * len(workload_names),
    ]
    for engine_id, engine in engines.items():
        if not isinstance(engine, dict):
            continue
        cells = [engine_id, _stats_text(engine.get("composite_score"))]
        workloads = engine.get("workloads", {})
        for workload in workload_names:
            entry = workloads.get(workload, {}) if isinstance(workloads, dict) else {}
            cells.append(_stats_text(entry.get("score") if isinstance(entry, dict) else None))
        lines.append("| " + " | ".join(cells) + " |")

    ratios = comparisons.get("engines", {})
    if isinstance(ratios, dict):
        lines.extend(["", "Composite candidate/reference ratios (median [CI]):"])
        for engine_id, entry in ratios.items():
            if not isinstance(entry, dict):
                continue
            lines.append(f"- {engine_id}: {_stats_text(entry.get('composite_candidate_over_reference_score_ratio'))}")
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("report", type=Path, help="completed engine-matrix JSON report")
    args = parser.parse_args()
    try:
        print(render(load_report(args.report)))
    except ReportError as error:
        parser.error(str(error))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

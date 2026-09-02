#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().with_name("check-engine-regression.py")
SPEC = importlib.util.spec_from_file_location("check_engine_regression", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
policy_check = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(policy_check)


def report(score: float, low: float, high: float) -> dict:
    return {
        "schema_version": 1,
        "status": "complete",
        "manifest": {"sha256": "a" * 64},
        "summary": {
            "engines": {
                "lumen-jit": {
                    "composite_score": {
                        "median": score,
                        "median_confidence_interval": [low, high],
                    },
                    "workloads": {
                        "regexp": {
                            "score": {
                                "median": score,
                                "median_confidence_interval": [low, high],
                            }
                        }
                    },
                }
            }
        },
    }


POLICY = {
    "schema_version": 1,
    "manifest_sha256": "a" * 64,
    "engine": "lumen-jit",
    "components": {
        "composite": {
            "baseline_score": 100,
            "minimum_score_ratio": 0.9,
            "reason": "twice the locked Phase 0 confidence variation",
        },
        "regexp": {
            "baseline_score": 100,
            "minimum_score_ratio": 0.97,
            "reason": "stable component floor with the documented 3% policy",
        },
    },
}


class PolicyTests(unittest.TestCase):
    def test_clear_regression_requires_confidence_interval_below_floor(self) -> None:
        results = policy_check.check(report(80, 79, 85), POLICY)
        self.assertEqual([item["status"] for item in results], ["regressed", "regressed"])

    def test_point_drop_with_overlapping_interval_is_inconclusive(self) -> None:
        results = policy_check.check(report(96, 90, 99), POLICY)
        self.assertEqual([item["status"] for item in results], ["pass", "inconclusive"])

    def test_matching_manifest_is_required(self) -> None:
        candidate = report(100, 100, 100)
        candidate["manifest"]["sha256"] = "b" * 64
        with self.assertRaises(policy_check.RegressionError):
            policy_check.check(candidate, POLICY)

    def test_cli_files_are_read_only_and_parseable(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            report_path = root / "report.json"
            policy_path = root / "policy.json"
            report_path.write_text(json.dumps(report(100, 100, 100)), encoding="utf-8")
            policy_path.write_text(json.dumps(POLICY), encoding="utf-8")
            self.assertEqual(policy_check.load_json(report_path)["status"], "complete")
            self.assertEqual(policy_check.load_json(policy_path)["engine"], "lumen-jit")


if __name__ == "__main__":
    unittest.main()

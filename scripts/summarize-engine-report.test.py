#!/usr/bin/env python3
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("summarize-engine-report.py")
SPEC = importlib.util.spec_from_file_location("summarize_engine_report", SCRIPT)
assert SPEC and SPEC.loader
summarizer = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(summarizer)


def stats(median: float, low: float, high: float) -> dict[str, object]:
    return {"median": median, "median_confidence_interval": [low, high]}


class SummarizerTest(unittest.TestCase):
    def report(self, status: str = "complete") -> dict[str, object]:
        return {
            "schema_version": 1,
            "status": status,
            "summary": {
                "comparisons": {
                    "reference_engine": "node",
                    "engines": {
                        "lumen-jit": {
                            "composite_candidate_over_reference_score_ratio": stats(0.8, 0.7, 0.9)
                        }
                    },
                },
                "engines": {
                    "node": {
                        "composite_score": stats(100.0, 99.0, 101.0),
                        "workloads": {"demo": {"score": stats(100.0, 99.0, 101.0)}},
                    },
                    "lumen-jit": {
                        "composite_score": stats(80.0, 79.0, 81.0),
                        "workloads": {"demo": {"score": stats(80.0, 79.0, 81.0)}},
                    },
                },
            },
        }

    def test_render_includes_medians_intervals_and_ratio(self) -> None:
        output = summarizer.render(self.report())
        self.assertIn("reference engine: node", output)
        self.assertIn("80.000 [79.000, 81.000]", output)
        self.assertIn("lumen-jit: 0.800 [0.700, 0.900]", output)

    def test_loader_rejects_incomplete_report(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "report.json"
            path.write_text(json.dumps(self.report("running")), encoding="utf-8")
            with self.assertRaises(summarizer.ReportError):
                summarizer.load_report(path)


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3

from __future__ import annotations

import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().with_name("bench-matrix.py")
SPEC = importlib.util.spec_from_file_location("bench_matrix", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
bench_matrix = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(bench_matrix)


class UnitTests(unittest.TestCase):
    def test_balanced_schedule_rotates_every_engine_through_each_position(self) -> None:
        schedule = bench_matrix.balanced_schedule(
            "measure", 6, ["one", "two"], ["a", "b", "c"], 17
        )
        counts: dict[tuple[str, int], int] = {}
        for entry in schedule:
            key = (entry["engine"], entry["engine_position"])
            counts[key] = counts.get(key, 0) + 1
        self.assertEqual(len(schedule), 36)
        self.assertEqual(set(counts.values()), {4})

    def test_bootstrap_summary_is_deterministic(self) -> None:
        first = bench_matrix.summary_stats([1, 2, 3, 4, 5], 0.95, 1000, 99)
        second = bench_matrix.summary_stats([1, 2, 3, 4, 5], 0.95, 1000, 99)
        self.assertEqual(first, second)
        self.assertEqual(first["median"], 3.0)
        self.assertEqual(first["count"], 5)
        self.assertIsNotNone(first["median_confidence_interval"])

    def test_score_parser_requires_the_requested_component(self) -> None:
        output = "Demo: 123\n----\nScore (version 7): 123\n"
        self.assertEqual(bench_matrix.parse_score(output, "Demo"), 123.0)
        with self.assertRaises(bench_matrix.BenchmarkError):
            bench_matrix.parse_score(output, "Other")

    def test_summary_retains_managed_memory_distributions_and_quality(self) -> None:
        metric_names = (
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
        samples = []
        for round_index, retained in enumerate((10, 20)):
            metrics = {name: 0 for name in metric_names}
            metrics["gc_pause_histogram"] = {
                "unit": "nanoseconds",
                "upper_bounds": [None],
                "counts": [0],
            }
            metrics["managed_memory"] = {
                "schema_version": 1,
                "complete": False,
                "managed_requested_bytes": {"bytes": retained, "quality": "lower_bound"},
                "managed_external_bytes": {"bytes": 4, "quality": "lower_bound"},
                "categories": {
                    "objects": {"bytes": retained, "quality": "exact"},
                    "side_tables": {"bytes": None, "quality": "unavailable"},
                },
            }
            samples.append(
                {
                    "phase": "measure",
                    "engine": "lumen",
                    "workload": "demo",
                    "round": round_index,
                    "score": 1,
                    "wall_seconds": 1,
                    "cpu_seconds": 1,
                    "peak_rss_bytes": 1,
                    "engine_metrics": metrics,
                }
            )
        summary = bench_matrix.summarize(
            samples, ["lumen"], ["demo"], "lumen", 0.95, 0, 1
        )
        managed = summary["engines"]["lumen"]["workloads"]["demo"]["engine_metrics"][
            "managed_memory"
        ]
        self.assertEqual(managed["managed_requested_bytes"]["bytes"]["median"], 15)
        self.assertEqual(managed["categories"]["objects"]["quality"], "exact")
        self.assertIsNone(managed["categories"]["side_tables"]["bytes"])

    def test_engine_metrics_parser_requires_one_versioned_json_line(self) -> None:
        stderr = (
            'noise\n[lumen-perf] {"schema_version":1,"jit_compile_seconds":0.25,'
            '"gc_collections":2,"gc_pause_histogram":{"unit":"nanoseconds",'
            '"upper_bounds":[50000,null],"counts":[1,1]},'
            '"managed_memory":{"schema_version":1,"agent_id":1,"heap_id":2,'
            '"safepoint":"post_gc",'
            '"complete":false,"managed_requested_bytes":{"bytes":12,"quality":"lower_bound",'
            '"reason":"partial"},'
            '"managed_external_bytes":{"bytes":4,"quality":"lower_bound","reason":"partial"},'
            '"categories":{"objects":{"bytes":8,"quality":"exact"},'
            '"side_tables":{"bytes":null,"quality":"unavailable","reason":"pending"}}}}\n'
        )
        metrics = bench_matrix.parse_engine_metrics(stderr, "[lumen-perf] ")
        self.assertEqual(metrics["jit_compile_seconds"], 0.25)
        self.assertEqual(sum(metrics["gc_pause_histogram"]["counts"]), 2)
        self.assertEqual(metrics["managed_memory"]["managed_requested_bytes"]["bytes"], 12)
        with self.assertRaises(bench_matrix.BenchmarkError):
            bench_matrix.parse_engine_metrics("", "[lumen-perf] ")
        with self.assertRaises(bench_matrix.BenchmarkError):
            bench_matrix.parse_engine_metrics(
                stderr.replace('"counts":[1,1]', '"counts":[1,0]'), "[lumen-perf] "
            )
        with self.assertRaises(bench_matrix.BenchmarkError):
            bench_matrix.parse_engine_metrics(
                stderr.replace(
                    '"side_tables":{"bytes":null,"quality":"unavailable","reason":"pending"}',
                    '"side_tables":{"bytes":0,"quality":"unavailable","reason":"pending"}',
                ),
                "[lumen-perf] ",
            )
        unavailable = (
            '[lumen-perf] {"schema_version":1,"gc_collections":0,'
            '"managed_memory":{"schema_version":1,"agent_id":1,"heap_id":2,'
            '"safepoint":null,"complete":false,'
            '"managed_requested_bytes":{"bytes":null,"quality":"unavailable",'
            '"reason":"no safepoint"},'
            '"managed_external_bytes":{"bytes":null,"quality":"unavailable",'
            '"reason":"no safepoint"}}}\n'
        )
        parsed = bench_matrix.parse_engine_metrics(unavailable, "[lumen-perf] ")
        self.assertIsNone(parsed["managed_memory"]["managed_requested_bytes"]["bytes"])


class IntegrationTest(unittest.TestCase):
    def test_offline_matrix_checkpoints_raw_samples_and_paired_ratio(self) -> None:
        with tempfile.TemporaryDirectory() as temporary_name:
            root = Path(temporary_name)
            fixtures = root / "fixtures"
            fixtures.mkdir()
            contents = {
                "base.js": b"// base\n",
                "demo.js": b"// workload\n",
                "run.js": b"load('ignored');\n// driver\n",
            }
            files = []
            for name, payload in contents.items():
                (fixtures / name).write_bytes(payload)
                files.append(
                    {
                        "path": name,
                        "sha256": hashlib.sha256(payload).hexdigest(),
                        "role": "workload" if name == "demo.js" else name.removesuffix(".js"),
                    }
                )

            fake_engine = root / "fake-engine.py"
            fake_engine.write_text(
                """#!/usr/bin/env python3
import sys
if '--version' in sys.argv:
    print('fake 1')
else:
    score = sys.argv[sys.argv.index('--score') + 1]
    print(f'Demo: {score}')
    print('----')
    print(f'Score (version 7): {score}')
""",
                encoding="utf-8",
            )
            fake_engine.chmod(0o755)
            manifest = {
                "schema_version": 1,
                "suite": {
                    "id": "fake",
                    "title": "fake",
                    "direction": "higher",
                    "fixture_root": "fixtures",
                    "source": {"revision": "test"},
                    "files": files,
                    "base": "base.js",
                    "driver": "run.js",
                    "workloads": [{"id": "demo", "label": "Demo", "file": "demo.js"}],
                },
                "run": {
                    "warmup_rounds": 0,
                    "sample_rounds": 2,
                    "timeout_seconds": 10,
                    "cpu_affinity": [],
                    "schedule_seed": 7,
                    "confidence_level": 0.95,
                    "bootstrap_resamples": 100,
                    "environment": {"LC_ALL": "C"},
                    "clear_environment_prefixes": ["LUMEN_"],
                },
                "reference_engine": "alpha",
                "engines": [
                    {
                        "id": "alpha",
                        "label": "alpha",
                        "program": "fake-engine.py",
                        "version_args": ["--version"],
                        "expected_version": "fake 1",
                        "input_mode": "combined",
                        "args": ["--score", "100"],
                        "allocator": "test",
                        "required": True,
                    },
                    {
                        "id": "beta",
                        "label": "beta",
                        "program": "fake-engine.py",
                        "version_args": ["--version"],
                        "expected_version": "fake 1",
                        "input_mode": "combined",
                        "args": ["--score", "200"],
                        "allocator": "test",
                        "required": True,
                    },
                ],
                "provenance": {"repositories": []},
            }
            manifest_path = root / "manifest.json"
            manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
            output = root / "result.json"
            completed = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT),
                    "--repo-root",
                    str(root),
                    "--manifest",
                    str(manifest_path),
                    "--output",
                    str(output),
                    "--no-build",
                    "--cpu",
                    "none",
                ],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                check=False,
                timeout=30,
            )
            self.assertEqual(
                completed.returncode,
                0,
                completed.stderr.decode("utf-8", errors="replace"),
            )
            report = json.loads(output.read_text(encoding="utf-8"))
            self.assertEqual(report["status"], "complete")
            self.assertEqual(len(report["samples"]), 4)
            self.assertTrue(all(sample["exit_code"] == 0 for sample in report["samples"]))
            ratio = report["summary"]["comparisons"]["engines"]["beta"][
                "composite_candidate_over_reference_score_ratio"
            ]
            self.assertAlmostEqual(ratio["median"], 2.0)
            self.assertEqual(
                report["derived_inputs"]["driver_sha256"],
                hashlib.sha256(b"// driver\n").hexdigest(),
            )


if __name__ == "__main__":
    unittest.main()

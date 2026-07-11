#!/usr/bin/env python3
"""Unit tests for scripts/perf/bench_track.py (stdlib unittest, no deps).

Run: python3 -m unittest scripts.perf.test_bench_track -v
     (or: cd scripts/perf && python3 -m unittest test_bench_track -v)
"""

from __future__ import annotations

import json
import pathlib
import tempfile
import unittest

import bench_track


class RecordShapeTests(unittest.TestCase):
    def test_build_record_schema(self):
        record = bench_track.build_record(
            "components", {"khive-score/score_ops.mean_ns": 123.4}, "a" * 40, "perf/bench-ci-tracking"
        )
        self.assertEqual(record["schema_version"], bench_track.SCHEMA_VERSION)
        self.assertEqual(record["suite"], "components")
        self.assertEqual(record["sha"], "a" * 40)
        self.assertEqual(record["branch"], "perf/bench-ci-tracking")
        self.assertIn("timestamp", record)
        self.assertIn("metrics", record)
        self.assertIn("host", record)
        self.assertIsInstance(record["host"], dict)
        self.assertIn("os", record["host"])

    def test_build_record_uses_commit_timestamp_not_wall_clock(self):
        # HEAD's commit timestamp must be a real git-derived ISO8601 string,
        # not e.g. empty or a raw epoch float.
        sha = bench_track._git_sha()
        record = bench_track.build_record("components", {"x": 1.0}, sha, "main")
        self.assertRegex(record["timestamp"], r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}")


class LedgerRoundTripTests(unittest.TestCase):
    def test_append_and_read_round_trip(self):
        with tempfile.TemporaryDirectory() as tmp:
            data_dir = pathlib.Path(tmp)
            record = bench_track.build_record("pipeline", {"mean_precision_at_k": 0.83}, "b" * 40, "main")
            path = bench_track.append_record(record, data_dir=data_dir)
            self.assertTrue(path.exists())

            back = bench_track.read_records("pipeline", data_dir=data_dir)
            self.assertEqual(len(back), 1)
            self.assertEqual(back[0]["sha"], "b" * 40)
            self.assertEqual(back[0]["metrics"]["mean_precision_at_k"], 0.83)

    def test_append_is_jsonl_one_record_per_line(self):
        with tempfile.TemporaryDirectory() as tmp:
            data_dir = pathlib.Path(tmp)
            for i in range(3):
                record = bench_track.build_record("load", {"n": float(i)}, f"{i:040d}", "main")
                bench_track.append_record(record, data_dir=data_dir)
            path = bench_track.ledger_path("load", data_dir)
            lines = path.read_text().splitlines()
            self.assertEqual(len(lines), 3)
            for line in lines:
                json.loads(line)  # each line stands alone as valid JSON

    def test_read_records_missing_ledger_returns_empty(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(bench_track.read_records("nonexistent", data_dir=pathlib.Path(tmp)), [])


class TrendMarkdownTests(unittest.TestCase):
    def test_render_empty_ledger(self):
        with tempfile.TemporaryDirectory() as tmp:
            md = bench_track.render_trend_markdown("components", data_dir=pathlib.Path(tmp))
            self.assertIn("No history yet.", md)

    def test_render_shows_direction_arrows(self):
        with tempfile.TemporaryDirectory() as tmp:
            data_dir = pathlib.Path(tmp)
            for i, val in enumerate([0.70, 0.75, 0.60]):
                record = bench_track.build_record("pipeline", {"mean_precision_at_k": val}, f"{i:040d}", "main")
                bench_track.append_record(record, data_dir=data_dir)
            md = bench_track.render_trend_markdown("pipeline", limit=10, data_dir=data_dir)
            self.assertIn("mean_precision_at_k", md)
            self.assertIn("down", md)  # last transition 0.75 -> 0.60
            self.assertNotIn(">= 0.70", md)  # never renders a threshold

    def test_render_respects_limit_window(self):
        with tempfile.TemporaryDirectory() as tmp:
            data_dir = pathlib.Path(tmp)
            for i in range(5):
                record = bench_track.build_record("components", {"m": float(i)}, f"{i:040d}", "main")
                bench_track.append_record(record, data_dir=data_dir)
            md = bench_track.render_trend_markdown("components", limit=2, data_dir=data_dir)
            self.assertIn("runs in window: 2 (of 5 total)", md)


class JsonSourceTests(unittest.TestCase):
    def test_collect_json_metrics_flattens_nested(self):
        with tempfile.TemporaryDirectory() as tmp:
            json_path = pathlib.Path(tmp) / "bench.json"
            json_path.write_text(
                json.dumps(
                    {
                        "dataset": "ci-synthetic",
                        "beam_growth_exponent": 0.31,
                        "rows": [
                            {"n": 10000, "recall_at_10": 0.94, "speedup_vs_brute_force": 12.5},
                            {"n": 50000, "recall_at_10": 0.92, "speedup_vs_brute_force": 18.2},
                        ],
                    }
                )
            )
            metrics = bench_track.collect_json_metrics(json_path)
            self.assertEqual(metrics["beam_growth_exponent"], 0.31)
            self.assertEqual(metrics["rows.0.recall_at_10"], 0.94)
            self.assertEqual(metrics["rows.1.speedup_vs_brute_force"], 18.2)
            self.assertNotIn("dataset", metrics)  # string leaf, not numeric

    def test_collect_json_metrics_empty_raises(self):
        with tempfile.TemporaryDirectory() as tmp:
            json_path = pathlib.Path(tmp) / "empty.json"
            json_path.write_text(json.dumps({"dataset": "x", "note": "no numbers here"}))
            with self.assertRaises(SystemExit):
                bench_track.collect_json_metrics(json_path)


class CriterionSourceTests(unittest.TestCase):
    def _write_estimates(self, path: pathlib.Path, mean_ns: float, median_ns: float, std_ns: float):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(
            json.dumps(
                {
                    "mean": {"point_estimate": mean_ns, "confidence_interval": {}},
                    "median": {"point_estimate": median_ns},
                    "std_dev": {"point_estimate": std_ns},
                }
            )
        )

    def test_collect_criterion_metrics_walks_tree(self):
        with tempfile.TemporaryDirectory() as tmp:
            criterion_dir = pathlib.Path(tmp) / "criterion"
            self._write_estimates(
                criterion_dir / "khive-score" / "score_ops" / "new" / "estimates.json", 100.0, 98.0, 5.0
            )
            self._write_estimates(
                criterion_dir / "khive-bm25" / "bm25_bench" / "new" / "estimates.json", 200.0, 199.0, 8.0
            )
            metrics = bench_track.collect_criterion_metrics(criterion_dir)
            self.assertEqual(metrics["khive-score/score_ops.mean_ns"], 100.0)
            self.assertEqual(metrics["khive-score/score_ops.median_ns"], 98.0)
            self.assertEqual(metrics["khive-score/score_ops.std_dev_ns"], 5.0)
            self.assertEqual(metrics["khive-bm25/bm25_bench.mean_ns"], 200.0)

    def test_collect_criterion_metrics_prefers_new_over_base(self):
        with tempfile.TemporaryDirectory() as tmp:
            criterion_dir = pathlib.Path(tmp) / "criterion"
            self._write_estimates(
                criterion_dir / "grp" / "bench" / "new" / "estimates.json", 111.0, 110.0, 1.0
            )
            self._write_estimates(
                criterion_dir / "grp" / "bench" / "base" / "estimates.json", 999.0, 998.0, 9.0
            )
            metrics = bench_track.collect_criterion_metrics(criterion_dir)
            self.assertEqual(metrics["grp/bench.mean_ns"], 111.0)

    def test_collect_criterion_metrics_empty_raises(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaises(SystemExit):
                bench_track.collect_criterion_metrics(pathlib.Path(tmp))


if __name__ == "__main__":
    unittest.main()

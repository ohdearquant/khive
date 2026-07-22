#!/usr/bin/env python3
"""Unit tests for scripts/perf/bench_track.py (stdlib unittest, no deps).

Run: python3 -m unittest scripts.perf.test_bench_track -v
     (or: cd scripts/perf && python3 -m unittest test_bench_track -v)
"""

from __future__ import annotations

import argparse
import json
import pathlib
import sys
import tempfile
import unittest

import bench_track
import bench_calibrate


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


class GateExitCodeTests(unittest.TestCase):
    def test_build_record_gate_pass(self):
        record = bench_track.build_record("bench-1m", {"recall_at_10": 0.94}, "a" * 40, "main", gate_exit_code=0)
        self.assertEqual(record["gate_exit_code"], 0)
        self.assertEqual(record["gate_status"], "pass")

    def test_build_record_gate_fail(self):
        record = bench_track.build_record("bench-1m", {"recall_at_10": 0.40}, "a" * 40, "main", gate_exit_code=1)
        self.assertEqual(record["gate_exit_code"], 1)
        self.assertEqual(record["gate_status"], "fail")

    def test_build_record_no_gate_by_default(self):
        record = bench_track.build_record("components", {"m": 1.0}, "a" * 40, "main")
        self.assertIsNone(record["gate_exit_code"])
        self.assertIsNone(record["gate_status"])

    def test_build_record_default_status_ok(self):
        record = bench_track.build_record("components", {"m": 1.0}, "a" * 40, "main")
        self.assertEqual(record["status"], "ok")
        self.assertIsNone(record["error"])


class ErrorRecordTests(unittest.TestCase):
    def test_build_error_record_shape(self):
        record = bench_track.build_error_record("pipeline", "c" * 40, "main", "boom: no output")
        self.assertEqual(record["status"], "error")
        self.assertEqual(record["error"], "boom: no output")
        self.assertEqual(record["metrics"], {})
        self.assertIsNone(record["gate_exit_code"])

    def test_cmd_record_writes_error_record_on_missing_json_file(self):
        with tempfile.TemporaryDirectory() as tmp:
            data_dir = pathlib.Path(tmp)
            args = argparse.Namespace(
                suite="bench-1m",
                source="json",
                json_file=str(data_dir / "does-not-exist.json"),
                json_prefix="",
                criterion_dir=None,
                extra_arg=[],
                run_dir=None,
                gate_exit_code=1,
                sha="d" * 40,
                branch="main",
                data_dir=str(data_dir),
                limit=10,
                summary_out=None,
            )
            rc = bench_track._cmd_record(args)
            self.assertEqual(rc, 1)

            records = bench_track.read_records("bench-1m", data_dir=data_dir)
            self.assertEqual(len(records), 1)
            self.assertEqual(records[0]["status"], "error")
            self.assertEqual(records[0]["metrics"], {})

    def test_cmd_record_writes_error_record_on_empty_criterion_dir(self):
        with tempfile.TemporaryDirectory() as tmp:
            data_dir = pathlib.Path(tmp)
            empty_criterion = data_dir / "criterion"
            empty_criterion.mkdir()
            args = argparse.Namespace(
                suite="components",
                source="criterion",
                json_file=None,
                json_prefix="",
                criterion_dir=str(empty_criterion),
                extra_arg=[],
                run_dir=None,
                gate_exit_code=None,
                sha="e" * 40,
                branch="main",
                data_dir=str(data_dir),
                limit=10,
                summary_out=None,
            )
            rc = bench_track._cmd_record(args)
            self.assertEqual(rc, 1)
            records = bench_track.read_records("components", data_dir=data_dir)
            self.assertEqual(len(records), 1)
            self.assertEqual(records[0]["status"], "error")
            self.assertIn("no criterion estimates.json", records[0]["error"])


class ShardAggregationTests(unittest.TestCase):
    """Reproduces the bug: 4 `components` shards append 4
    partial records for the same sha; the trend summary must render one
    logical run per sha with the full union of every shard's metrics, not
    one row per shard with only the last shard's metric names.
    """

    def test_two_sha_two_shard_aggregates_to_two_runs_with_full_metric_union(self):
        with tempfile.TemporaryDirectory() as tmp:
            data_dir = pathlib.Path(tmp)
            shas = ["a" * 40, "b" * 40]
            for sha in shas:
                shard1 = bench_track.build_record(
                    "components", {"khive-score/score_ops.mean_ns": 100.0}, sha, "main"
                )
                shard2 = bench_track.build_record(
                    "components", {"khive-hnsw/hnsw_build.mean_ns": 200.0}, sha, "main"
                )
                bench_track.append_record(shard1, data_dir=data_dir)
                bench_track.append_record(shard2, data_dir=data_dir)

            raw_records = bench_track.read_records("components", data_dir=data_dir)
            self.assertEqual(len(raw_records), 4)  # 2 shas x 2 shards, still stored as 4 raw rows

            aggregated = bench_track._aggregate_shards(raw_records)
            self.assertEqual(len(aggregated), 2)  # but only 2 logical runs (one per sha)
            for rec in aggregated:
                self.assertIn("khive-score/score_ops.mean_ns", rec["metrics"])
                self.assertIn("khive-hnsw/hnsw_build.mean_ns", rec["metrics"])

            md = bench_track.render_trend_markdown("components", data_dir=data_dir)
            self.assertIn("runs in window: 2 (of 2 total)", md)
            self.assertIn("khive-score/score_ops.mean_ns", md)
            self.assertIn("khive-hnsw/hnsw_build.mean_ns", md)

    def test_aggregate_shards_preserves_chronological_order(self):
        records = [
            {"sha": "a" * 40, "metrics": {"x": 1.0}},
            {"sha": "b" * 40, "metrics": {"y": 2.0}},
            {"sha": "a" * 40, "metrics": {"z": 3.0}},
        ]
        aggregated = bench_track._aggregate_shards(records)
        self.assertEqual([r["sha"] for r in aggregated], ["a" * 40, "b" * 40])
        self.assertEqual(aggregated[0]["metrics"], {"x": 1.0, "z": 3.0})

    def test_aggregate_shards_error_status_not_masked_by_later_ok_shard(self):
        records = [
            bench_track.build_error_record("components", "a" * 40, "main", "shard 1 build failed"),
            bench_track.build_record("components", {"khive-hnsw/hnsw_build.mean_ns": 200.0}, "a" * 40, "main"),
        ]
        aggregated = bench_track._aggregate_shards(records)
        self.assertEqual(len(aggregated), 1)
        self.assertEqual(aggregated[0]["status"], "error")
        self.assertEqual(aggregated[0]["error"], "shard 1 build failed")
        self.assertIn("khive-hnsw/hnsw_build.mean_ns", aggregated[0]["metrics"])


class TimeoutRecordTests(unittest.TestCase):
    """`_run_once_no_gate` re-raises `subprocess.TimeoutExpired`
    on a suite that runs past its timeout, but `_cmd_record`'s except tuple
    did not catch it - a timed-out suite raised straight past `_cmd_record`
    instead of leaving a status=error ledger row. Registers a synthetic
    bench_calibrate suite whose child sleeps well past a 1s timeout.
    """

    SUITE_NAME = "_test_timeout_suite"

    def setUp(self):
        def build_cmd(run_dir, extra_args):
            return [sys.executable, "-c", "import time; time.sleep(30)"]

        def extract(run_dir, proc):
            return {"unused": 0.0}

        bench_calibrate.SUITES[self.SUITE_NAME] = {
            "build_cmd": build_cmd,
            "extract": extract,
            "default_args": [],
            "timeout_s": 1,
        }
        self.addCleanup(bench_calibrate.SUITES.pop, self.SUITE_NAME, None)

    def test_timeout_writes_error_record_instead_of_raising(self):
        with tempfile.TemporaryDirectory() as tmp:
            data_dir = pathlib.Path(tmp)
            args = argparse.Namespace(
                suite=self.SUITE_NAME,
                source="calibrate",
                json_file=None,
                json_prefix="",
                criterion_dir=None,
                extra_arg=[],
                run_dir=str(data_dir / "run"),
                gate_exit_code=None,
                run_id=None,
                run_attempt=None,
                sha="f" * 40,
                branch="main",
                data_dir=str(data_dir),
                limit=10,
                summary_out=None,
            )
            rc = bench_track._cmd_record(args)
            self.assertEqual(rc, 1)

            records = bench_track.read_records(self.SUITE_NAME, data_dir=data_dir)
            self.assertEqual(len(records), 1)
            self.assertEqual(records[0]["status"], "error")
            self.assertIn("timed out", records[0]["error"].lower())


class RunIdAggregationTests(unittest.TestCase):
    """Keying `_aggregate_shards` on sha alone lets a rerun
    of the same commit (same sha, new run_id) merge into the FIRST run's
    row - a pass-then-fail rerun then produces one logical run whose
    metrics came from the failing rerun but whose gate_status stayed
    "pass" from the first run. Keying on (sha, run_id, run_attempt) keeps
    every workflow run - including reruns of the same sha - as its own row.
    """

    def test_same_sha_different_run_id_stays_separate(self):
        with tempfile.TemporaryDirectory() as tmp:
            data_dir = pathlib.Path(tmp)
            sha = "a" * 40
            rec1 = bench_track.build_record("components", {"m": 1.0}, sha, "main", run_id="100", run_attempt="1")
            rec2 = bench_track.build_record("components", {"m": 2.0}, sha, "main", run_id="200", run_attempt="1")
            bench_track.append_record(rec1, data_dir=data_dir)
            bench_track.append_record(rec2, data_dir=data_dir)
            aggregated = bench_track._aggregate_shards(bench_track.read_records("components", data_dir=data_dir))
            self.assertEqual(len(aggregated), 2)
            self.assertEqual({r["metrics"]["m"] for r in aggregated}, {1.0, 2.0})

    def test_same_sha_same_run_id_different_attempt_stays_separate(self):
        with tempfile.TemporaryDirectory() as tmp:
            data_dir = pathlib.Path(tmp)
            sha = "b" * 40
            rec1 = bench_track.build_record("components", {"m": 1.0}, sha, "main", run_id="100", run_attempt="1")
            rec2 = bench_track.build_record("components", {"m": 2.0}, sha, "main", run_id="100", run_attempt="2")
            bench_track.append_record(rec1, data_dir=data_dir)
            bench_track.append_record(rec2, data_dir=data_dir)
            aggregated = bench_track._aggregate_shards(bench_track.read_records("components", data_dir=data_dir))
            self.assertEqual(len(aggregated), 2)

    def test_same_sha_same_run_id_same_attempt_still_merges_like_shards(self):
        with tempfile.TemporaryDirectory() as tmp:
            data_dir = pathlib.Path(tmp)
            sha = "c" * 40
            rec1 = bench_track.build_record("components", {"a": 1.0}, sha, "main", run_id="100", run_attempt="1")
            rec2 = bench_track.build_record("components", {"b": 2.0}, sha, "main", run_id="100", run_attempt="1")
            bench_track.append_record(rec1, data_dir=data_dir)
            bench_track.append_record(rec2, data_dir=data_dir)
            aggregated = bench_track._aggregate_shards(bench_track.read_records("components", data_dir=data_dir))
            self.assertEqual(len(aggregated), 1)
            self.assertEqual(aggregated[0]["metrics"], {"a": 1.0, "b": 2.0})

    def test_duplicate_metric_name_within_run_last_write_wins_and_flagged(self):
        with tempfile.TemporaryDirectory() as tmp:
            data_dir = pathlib.Path(tmp)
            sha = "d" * 40
            rec1 = bench_track.build_record("components", {"dup": 1.0}, sha, "main", run_id="100", run_attempt="1")
            rec2 = bench_track.build_record("components", {"dup": 2.0}, sha, "main", run_id="100", run_attempt="1")
            bench_track.append_record(rec1, data_dir=data_dir)
            bench_track.append_record(rec2, data_dir=data_dir)
            aggregated = bench_track._aggregate_shards(bench_track.read_records("components", data_dir=data_dir))
            self.assertEqual(len(aggregated), 1)
            self.assertEqual(aggregated[0]["metrics"]["dup"], 2.0)
            self.assertIn("dup", aggregated[0]["metric_collisions"])

    def test_rerun_same_sha_different_run_id_does_not_mix_gate_status(self):
        """Reproduces the exact scenario: pass-then-fail rerun of
        the same sha must not blend into one row with mismatched gate
        provenance."""
        with tempfile.TemporaryDirectory() as tmp:
            data_dir = pathlib.Path(tmp)
            sha = "e" * 40
            passing = bench_track.build_record(
                "bench-1m", {"recall_at_10": 0.95}, sha, "main", gate_exit_code=0, run_id="1", run_attempt="1"
            )
            failing = bench_track.build_record(
                "bench-1m", {"recall_at_10": 0.10}, sha, "main", gate_exit_code=1, run_id="2", run_attempt="1"
            )
            bench_track.append_record(passing, data_dir=data_dir)
            bench_track.append_record(failing, data_dir=data_dir)
            aggregated = bench_track._aggregate_shards(bench_track.read_records("bench-1m", data_dir=data_dir))
            self.assertEqual(len(aggregated), 2)
            by_run = {r["run_id"]: r for r in aggregated}
            self.assertEqual(by_run["1"]["gate_status"], "pass")
            self.assertEqual(by_run["1"]["metrics"]["recall_at_10"], 0.95)
            self.assertEqual(by_run["2"]["gate_status"], "fail")
            self.assertEqual(by_run["2"]["metrics"]["recall_at_10"], 0.10)

    def test_missing_run_id_defaults_do_not_break_legacy_records(self):
        # Bare dict records without run_id/run_attempt keys (e.g. records
        # written before schema_version 2) must still aggregate by sha.
        records = [
            {"sha": "f" * 40, "metrics": {"x": 1.0}},
            {"sha": "f" * 40, "metrics": {"y": 2.0}},
        ]
        aggregated = bench_track._aggregate_shards(records)
        self.assertEqual(len(aggregated), 1)
        self.assertEqual(aggregated[0]["metrics"], {"x": 1.0, "y": 2.0})


class CalibrateNoGateTests(unittest.TestCase):
    """collect_calibrate_metrics must record metrics from a run whose
    internal threshold FAILED (nonzero child exit) - it is a tracker, not a
    gate. Registers a synthetic bench_calibrate suite whose child process
    always exits 1 (simulating a suite's own "Gate: FAIL" -> exit 1) and
    asserts the metric still gets extracted
    instead of an exception propagating.
    """

    SUITE_NAME = "_test_regressed_suite"

    def setUp(self):
        def build_cmd(run_dir, extra_args):
            return [
                sys.executable,
                "-c",
                "print('metric_value=42.0'); import sys; sys.exit(1)",
            ]

        def extract(run_dir, proc):
            for line in proc.stdout.splitlines():
                if line.startswith("metric_value="):
                    return {"metric_value": float(line.split("=", 1)[1])}
            return {}

        bench_calibrate.SUITES[self.SUITE_NAME] = {
            "build_cmd": build_cmd,
            "extract": extract,
            "default_args": [],
            "timeout_s": 30,
        }
        self.addCleanup(bench_calibrate.SUITES.pop, self.SUITE_NAME, None)

    def test_regressed_run_still_records_metrics(self):
        with tempfile.TemporaryDirectory() as tmp:
            run_dir = pathlib.Path(tmp) / "run"
            metrics = bench_track.collect_calibrate_metrics(self.SUITE_NAME, [], run_dir)
            self.assertEqual(metrics["metric_value"], 42.0)
            self.assertEqual(metrics["_exit_code"], 1.0)
            self.assertIn("_wall_s", metrics)


if __name__ == "__main__":
    unittest.main()

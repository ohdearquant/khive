#!/usr/bin/env python3
"""Unit tests for the bench-overhaul PR 1 manifest and coverage validator
(stdlib unittest, no deps).

Run: python3 -m unittest scripts.perf.test_flagship_coverage -v
     (or: cd scripts/perf && python3 -m unittest test_flagship_coverage -v)
"""

from __future__ import annotations

import datetime
import json
import pathlib
import tempfile
import unittest

import coverage_validator
import flagship_schema

MANIFEST_PATH = pathlib.Path(__file__).parent / "flagship_workloads.toml"


def _base_scenario(**overrides) -> dict:
    scenario = {
        "scenario_id": "f1.recall.warm.real",
        "feature": "F1",
        "surface": "mcp_daemon",
        "operation": "memory.recall",
        "fixture": "memory_12k_sentinel_settled",
        "fixture_hash": "sha256:" + "a" * 64,
        "scale": {"memories": 12000},
        "embedder": "production",
        "state": "warm",
        "settle": "sequential_sentinel",
        "attempts": 1000,
        "concurrency": 1,
        "required_percentiles": ["p50", "p95", "p99"],
        "request_deadline_ms": 30000,
        "request_deadline_provenance": "harness safety cap",
        "runner_class": "self_hosted_real_embedder",
    }
    scenario.update(overrides)
    return scenario


def _base_record(**overrides) -> dict:
    record = {
        "schema_version": 1,
        "suite": "flagship-e2e",
        "scenario_id": "f1.recall.warm.real",
        "feature": "F1",
        "sha": "a" * 40,
        "branch": "main",
        "run_id": "123",
        "run_attempt": "1",
        "timestamp": "2026-07-11T00:00:00+00:00",
        "status": "ok",
        "metrics": {"p50_us": 1200.0},
        "distributions": {},
        "workload": {
            "scenario_id": "f1.recall.warm.real",
            "fixture": "memory_12k_sentinel_settled",
            "fixture_hash": "sha256:" + "a" * 64,
            "scale": {"memories": 12000},
            "concurrency": 1,
            "attempts": 1000,
        },
        "runtime": {
            "surface": "mcp_daemon",
            "embedder": "production",
            "runner_class": "self_hosted_real_embedder",
            "daemon_fallback_count": 0,
        },
        "host": {"os": "Linux", "arch": "x86_64", "cpu_count": 8},
        "settle": {"method": "sequential_sentinel", "state": "warm"},
        "calibration": None,
        "artifact": {"name": "report.json", "sha256": "b" * 64},
    }
    record.update(overrides)
    return record


class ManifestTests(unittest.TestCase):
    def test_real_manifest_loads_with_no_errors(self):
        data, errors = coverage_validator.load_manifest(MANIFEST_PATH)
        self.assertEqual(errors, [])
        self.assertGreater(len(data["scenario"]), 0)

    def test_every_feature_has_at_least_one_scenario(self):
        data, _ = coverage_validator.load_manifest(MANIFEST_PATH)
        by_feature = coverage_validator._required_scenario_ids_by_feature(data["scenario"])
        for feature in flagship_schema.FEATURES:
            self.assertTrue(by_feature.get(feature), f"{feature} has zero manifest scenarios")

    def test_no_duplicate_scenario_ids_in_real_manifest(self):
        data, _ = coverage_validator.load_manifest(MANIFEST_PATH)
        ids = [sc["scenario_id"] for sc in data["scenario"]]
        self.assertEqual(len(ids), len(set(ids)))

    def test_oq_rulings_present(self):
        data, _ = coverage_validator.load_manifest(MANIFEST_PATH)
        rulings = data["oq_rulings"]
        self.assertEqual(rulings["oq2_measurement_deadline_ms"], 30000)
        self.assertEqual(rulings["oq2_mcp_client_default_timeout_ms"], 300000)
        self.assertEqual(rulings["oq7_freshness_days_self_hosted"], 14)
        self.assertEqual(rulings["oq7_freshness_days_hosted"], 7)

    def _write_manifest(self, tmp: pathlib.Path, scenarios: list[dict]) -> pathlib.Path:
        lines = []
        for sc in scenarios:
            lines.append("[[scenario]]")
            for key, value in sc.items():
                if isinstance(value, str):
                    lines.append(f'{key} = "{value}"')
                elif isinstance(value, dict):
                    inner = ", ".join(
                        f'{k} = "{v}"' if isinstance(v, str) else f"{k} = {v}" for k, v in value.items()
                    )
                    lines.append(f"{key} = {{ {inner} }}")
                elif isinstance(value, list):
                    inner = ", ".join(f'"{v}"' for v in value)
                    lines.append(f"{key} = [{inner}]")
                else:
                    lines.append(f"{key} = {value}")
            lines.append("")
        path = tmp / "manifest.toml"
        path.write_text("\n".join(lines) + "\n")
        return path

    def test_duplicate_scenario_id_is_flagged(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = self._write_manifest(
                pathlib.Path(tmp),
                [_base_scenario(scenario_id="f1.dup.warm.real"), _base_scenario(scenario_id="f1.dup.warm.real")],
            )
            _, errors = coverage_validator.load_manifest(path)
            self.assertTrue(any("duplicate scenario_id" in e for e in errors), errors)

    def test_invalid_surface_is_flagged(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = self._write_manifest(pathlib.Path(tmp), [_base_scenario(surface="local_dispatch")])
            _, errors = coverage_validator.load_manifest(path)
            self.assertTrue(any("invalid surface" in e for e in errors), errors)

    def test_malformed_fixture_hash_is_flagged(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = self._write_manifest(pathlib.Path(tmp), [_base_scenario(fixture_hash="md5:deadbeef")])
            _, errors = coverage_validator.load_manifest(path)
            self.assertTrue(any("fixture_hash must match" in e for e in errors), errors)

    def test_scenario_id_convention_is_enforced(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = self._write_manifest(pathlib.Path(tmp), [_base_scenario(scenario_id="not-a-valid-id")])
            _, errors = coverage_validator.load_manifest(path)
            self.assertTrue(any("does not match" in e for e in errors), errors)


class SchemaValidationTests(unittest.TestCase):
    def test_valid_record_has_no_errors(self):
        errors = flagship_schema.validate_record(_base_record())
        self.assertEqual(errors, [])

    def test_missing_required_field_is_flagged(self):
        record = _base_record()
        del record["workload"]
        errors = flagship_schema.validate_record(record)
        self.assertTrue(any(e.startswith("workload:") for e in errors), errors)

    def test_bad_status_enum_is_flagged(self):
        errors = flagship_schema.validate_record(_base_record(status="timed_out"))
        self.assertTrue(any(e.startswith("status:") for e in errors), errors)

    def test_distribution_percentile_ordering_is_checked(self):
        dist = {
            "estimator": "nearest_rank_v1",
            "unit": "us",
            "attempts": 1000,
            "successes": 1000,
            "timed_out": 0,
            "errors_by_code": {},
            "histogram_edges_us": [0, 1000],
            "histogram_counts": [500, 500],
            "p50_us": 900,
            "p95_us": 800,
            "p99_us": 1200,
            "max_us": 1300,
            "conditional_on_success": True,
        }
        errors = flagship_schema.validate_distribution(dist)
        self.assertTrue(any("p50_us <= p95_us <= p99_us" in e for e in errors), errors)

    def test_distribution_attempts_bound_is_checked(self):
        dist = {
            "estimator": "nearest_rank_v1",
            "unit": "us",
            "attempts": 10,
            "successes": 8,
            "timed_out": 5,
            "errors_by_code": {},
            "histogram_edges_us": [],
            "histogram_counts": [],
            "p50_us": None,
            "p95_us": None,
            "p99_us": None,
            "max_us": None,
            "conditional_on_success": True,
        }
        errors = flagship_schema.validate_distribution(dist)
        self.assertTrue(any("must not exceed attempts" in e for e in errors), errors)


class CoverageStatusTests(unittest.TestCase):
    NOW = datetime.datetime(2026, 7, 11, tzinfo=datetime.timezone.utc)

    def test_zero_percent_measured_with_no_records(self):
        scenario = _base_scenario()
        report = coverage_validator.compute_coverage({"scenario": [scenario]}, [], self.NOW)
        self.assertEqual(report["percent_measured"], 0.0)
        self.assertEqual(report["counts"]["missing"], 1)
        self.assertEqual(report["counts"]["measured"], 0)

    def test_zero_percent_measured_holds_across_the_real_manifest(self):
        data, _ = coverage_validator.load_manifest(MANIFEST_PATH)
        report = coverage_validator.compute_coverage(data, [], self.NOW)
        self.assertEqual(report["percent_measured"], 0.0)
        self.assertEqual(report["counts"]["missing"], report["total_scenarios"])
        self.assertEqual(sorted(report["features_with_zero_measured"]), sorted(flagship_schema.FEATURES))

    def test_fresh_matching_record_is_measured(self):
        scenario = _base_scenario()
        record = _base_record(timestamp="2026-07-10T00:00:00+00:00")
        status, _ = coverage_validator.scenario_status(scenario, [record], self.NOW)
        self.assertEqual(status, "measured")

    def test_self_hosted_scenario_stale_after_14_days(self):
        scenario = _base_scenario(runner_class="self_hosted_real_embedder")
        record = _base_record(timestamp="2026-06-20T00:00:00+00:00")  # 21 days before NOW
        status, _ = coverage_validator.scenario_status(scenario, [record], self.NOW)
        self.assertEqual(status, "stale")

    def test_self_hosted_scenario_measured_within_14_days(self):
        scenario = _base_scenario(runner_class="self_hosted_real_embedder")
        record = _base_record(timestamp="2026-06-28T00:00:00+00:00")  # 13 days before NOW
        status, _ = coverage_validator.scenario_status(scenario, [record], self.NOW)
        self.assertEqual(status, "measured")

    def test_hosted_scenario_stale_after_7_days(self):
        scenario = _base_scenario(runner_class="hosted_hash")
        record = _base_record(
            runtime={
                "surface": "mcp_daemon",
                "embedder": "bench_hash",
                "runner_class": "hosted_hash",
                "daemon_fallback_count": 0,
            },
            timestamp="2026-07-02T00:00:00+00:00",  # 9 days before NOW
        )
        status, _ = coverage_validator.scenario_status(scenario, [record], self.NOW)
        self.assertEqual(status, "stale")

    def test_hosted_scenario_measured_within_7_days(self):
        scenario = _base_scenario(runner_class="hosted_hash")
        record = _base_record(
            runtime={
                "surface": "mcp_daemon",
                "embedder": "bench_hash",
                "runner_class": "hosted_hash",
                "daemon_fallback_count": 0,
            },
            timestamp="2026-07-06T00:00:00+00:00",  # 5 days before NOW
        )
        status, _ = coverage_validator.scenario_status(scenario, [record], self.NOW)
        self.assertEqual(status, "measured")

    def test_wrong_surface_is_detected(self):
        scenario = _base_scenario(surface="mcp_daemon")
        record = _base_record(
            timestamp="2026-07-10T00:00:00+00:00",
            runtime={
                "surface": "admin_cli",
                "embedder": "production",
                "runner_class": "self_hosted_real_embedder",
                "daemon_fallback_count": 0,
            },
        )
        status, _ = coverage_validator.scenario_status(scenario, [record], self.NOW)
        self.assertEqual(status, "wrong-surface")

    def test_record_status_confounded_is_propagated(self):
        scenario = _base_scenario()
        record = _base_record(status="confounded", timestamp="2026-07-10T00:00:00+00:00")
        status, _ = coverage_validator.scenario_status(scenario, [record], self.NOW)
        self.assertEqual(status, "confounded")

    def test_fixture_hash_mismatch_is_confounded(self):
        scenario = _base_scenario(fixture_hash="sha256:" + "c" * 64)
        record = _base_record(timestamp="2026-07-10T00:00:00+00:00")
        status, _ = coverage_validator.scenario_status(scenario, [record], self.NOW)
        self.assertEqual(status, "confounded")

    def test_missing_when_no_record_for_scenario_id(self):
        scenario = _base_scenario(scenario_id="f1.recall.cold_ann_overlap.real")
        record = _base_record()  # scenario_id defaults to f1.recall.warm.real
        status, _ = coverage_validator.scenario_status(scenario, [record], self.NOW)
        self.assertEqual(status, "missing")

    def test_latest_record_wins_when_multiple_exist(self):
        scenario = _base_scenario()
        older = _base_record(timestamp="2026-05-01T00:00:00+00:00")  # well outside 14d -> would be stale
        newer = _base_record(timestamp="2026-07-10T00:00:00+00:00")
        status, _ = coverage_validator.scenario_status(scenario, [older, newer], self.NOW)
        self.assertEqual(status, "measured")


class RecordsLoadingTests(unittest.TestCase):
    def test_load_records_returns_empty_list_for_missing_dir(self):
        records = coverage_validator.load_records(pathlib.Path("/nonexistent/path/does/not/exist"))
        self.assertEqual(records, [])

    def test_load_records_returns_empty_list_for_none(self):
        self.assertEqual(coverage_validator.load_records(None), [])

    def test_load_records_reads_jsonl_and_filters_by_suite(self):
        with tempfile.TemporaryDirectory() as tmp:
            data_dir = pathlib.Path(tmp)
            path = data_dir / "flagship-e2e.jsonl"
            other_suite = dict(_base_record())
            other_suite["suite"] = "pipeline"
            with path.open("w") as fh:
                fh.write(json.dumps(_base_record()) + "\n")
                fh.write(json.dumps(other_suite) + "\n")
            records = coverage_validator.load_records(data_dir)
            self.assertEqual(len(records), 1)
            self.assertEqual(records[0]["suite"], "flagship-e2e")


if __name__ == "__main__":
    unittest.main()

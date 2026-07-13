#!/usr/bin/env python3
"""Unit tests for the bench-overhaul PR 1 manifest and coverage validator
(stdlib unittest, no deps).

Run: python3 -m unittest scripts.perf.test_flagship_coverage -v
     (or: cd scripts/perf && python3 -m unittest test_flagship_coverage -v)
"""

from __future__ import annotations

import datetime
import hashlib
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
        "schema_version": 3,
        "suite": "flagship-e2e",
        "scenario_id": "f1.recall.warm.real",
        "feature": "F1",
        "operation": "memory.recall",
        "arm": "warm",
        "sha": "a" * 40,
        "branch": "main",
        "run_id": "123",
        "run_attempt": "1",
        "timestamp": "2026-07-11T00:00:00+00:00",
        "status": "ok",
        "metrics": {"p50_us": 1200.0},
        "distributions": {
            "latency": {
                "estimator": "nearest_rank_v1",
                "unit": "us",
                "attempts": 1000,
                "successes": 1000,
                "timed_out": 0,
                "errors_by_code": {},
                "histogram_edges_us": [0, 1000, 2000],
                "histogram_counts": [200, 700, 100],
                "p50_us": 1200,
                "p95_us": 1800,
                "p99_us": 1950,
                "max_us": 2000,
                "conditional_on_success": True,
            }
        },
        "workload": {
            "manifest_version": "1",
            "manifest_hash": "sha256:" + "d" * 64,
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
        # khive#946 Amendment 1 §2: client censor must strictly exceed the
        # 30000 ms server-side recall deadline (#919), so the harness cap is
        # 45000 ms, not 30000 ms.
        self.assertEqual(rulings["oq2_measurement_deadline_ms"], 45000)
        self.assertEqual(rulings["oq2_server_recall_deadline_ms"], 30000)
        self.assertEqual(rulings["oq2_mcp_client_default_timeout_ms"], 300000)
        self.assertEqual(rulings["oq7_freshness_days_self_hosted"], 14)
        self.assertEqual(rulings["oq7_freshness_days_hosted"], 7)

    def test_oq1_admin_surface_scenario_ids_match_manifest(self):
        """khive#946 Amendment 1 §2: exactly two admin-surface exceptions are
        named and bounded (F6 git-ingest/code-ingest, F10 daemon-control
        probe) - every other scenario is real-MCP coverage by default."""
        data, _ = coverage_validator.load_manifest(MANIFEST_PATH)
        rulings = data["oq_rulings"]
        admin_surface_ids = set(rulings["oq1_admin_surface_scenario_ids"])
        self.assertEqual(
            admin_surface_ids,
            {"f6.git_ingest.cli.production", "f6.code_ingest.cli.production", "f10.daemon.probe_only.floor"},
        )
        by_id = {sc["scenario_id"]: sc for sc in data["scenario"]}
        for scenario_id in admin_surface_ids:
            self.assertIn(scenario_id, by_id)
            self.assertIn(by_id[scenario_id]["surface"], ("admin_cli", "raw_daemon_control"))
        for scenario_id, sc in by_id.items():
            if scenario_id not in admin_surface_ids:
                self.assertEqual(sc["surface"], "mcp_daemon", f"{scenario_id} is not in the admin-surface exception list but is not mcp_daemon either")

    def test_f1_and_f4_have_500k_cohort(self):
        """Amendment 1 §7: F1 and F4 scenario sets gain a 500K cohort before
        any "exact cohort" claim is made."""
        data, _ = coverage_validator.load_manifest(MANIFEST_PATH)
        scale_by_id = {sc["scenario_id"]: sc["scale"] for sc in data["scenario"]}
        self.assertEqual(scale_by_id.get("f1.recall.warm.real.n500k", {}).get("memories"), 500000)
        self.assertEqual(scale_by_id.get("f4.traverse.powerlaw.n500000.depth3.real", {}).get("nodes"), 500000)

    def test_every_timed_scenario_client_censor_exceeds_server_deadline(self):
        """Amendment 1 §2 "Deadline classifiability": client-side censor
        must strictly exceed the 30000 ms server-side recall deadline
        (#919) so a timeout is attributable to exactly one deadline."""
        data, _ = coverage_validator.load_manifest(MANIFEST_PATH)
        server_deadline_ms = data["oq_rulings"]["oq2_server_recall_deadline_ms"]
        for sc in data["scenario"]:
            if sc["surface"] == "admin_cli":
                continue
            self.assertGreater(
                sc["request_deadline_ms"],
                server_deadline_ms,
                f"{sc['scenario_id']}: request_deadline_ms {sc['request_deadline_ms']} must strictly exceed "
                f"the {server_deadline_ms} ms server-side recall deadline",
            )

    def _write_manifest(self, tmp: pathlib.Path, scenarios: list[dict]) -> pathlib.Path:
        lines = []
        for sc in scenarios:
            lines.append("[[scenario]]")
            for key, value in sc.items():
                if isinstance(value, bool):
                    lines.append(f"{key} = {'true' if value else 'false'}")
                elif isinstance(value, str):
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

    def test_malformed_min_successes_is_flagged(self):
        """khive#945 r2: min_successes is the signed-amendment floor control -
        a malformed TOML value must be a manifest error, not a coverage crash."""
        for bad in ("500", True, 0, -5):
            with tempfile.TemporaryDirectory() as tmp:
                path = self._write_manifest(pathlib.Path(tmp), [_base_scenario(min_successes=bad)])
                _, errors = coverage_validator.load_manifest(path)
                self.assertTrue(
                    any("min_successes must be a positive integer" in e for e in errors), (bad, errors)
                )

    def test_f3_context_full_18_point_grid_is_present(self):
        """F3's {anchor} x {hops} x {budget} grid is 2 x 3 x 3 = 18 points
        (DESIGN.md "Required target scenarios" for F3). The manifest may put
        a reduced subset on the hosted runner tier, but the full 18-point
        denominator must exist - no grid point may be silently dropped."""
        data, _ = coverage_validator.load_manifest(MANIFEST_PATH)
        f3_ids = {sc["scenario_id"] for sc in data["scenario"] if sc["feature"] == "F3"}
        expected = {
            f"f3.context.{anchor}.hop{hop}.budget{budget}.real"
            for anchor in ("query", "entity_ids")
            for hop in (0, 1, 2)
            for budget in ("1k", "4k", "16k")
        }
        self.assertEqual(f3_ids, expected)
        self.assertEqual(len(expected), 18)


class SchemaValidationTests(unittest.TestCase):
    def test_valid_record_has_no_errors(self):
        errors = flagship_schema.validate_record(_base_record())
        self.assertEqual(errors, [])

    def test_schema_version_1_is_rejected(self):
        errors = flagship_schema.validate_record(_base_record(schema_version=1))
        self.assertTrue(any(e.startswith("schema_version:") for e in errors), errors)

    def test_schema_version_3_is_accepted(self):
        errors = flagship_schema.validate_record(_base_record(schema_version=3))
        self.assertEqual(errors, [])

    def test_missing_required_field_is_flagged(self):
        record = _base_record()
        del record["workload"]
        errors = flagship_schema.validate_record(record)
        self.assertTrue(any(e.startswith("workload:") for e in errors), errors)

    def test_missing_daemon_fallback_count_is_flagged(self):
        record = _base_record()
        del record["runtime"]["daemon_fallback_count"]
        errors = flagship_schema.validate_record(record)
        self.assertTrue(any("daemon_fallback_count" in e for e in errors), errors)

    def test_negative_daemon_fallback_count_is_flagged(self):
        record = _base_record()
        record["runtime"]["daemon_fallback_count"] = -1
        errors = flagship_schema.validate_record(record)
        self.assertTrue(any("daemon_fallback_count" in e for e in errors), errors)

    def test_empty_distributions_is_rejected(self):
        """khive#945 item 1/5: the validator's own former positive fixture
        used `distributions: {}` - that must now fail schema validation."""
        record = _base_record(distributions={})
        errors = flagship_schema.validate_record(record)
        self.assertTrue(any("distributions" in e and "at least one distribution" in e for e in errors), errors)

    def test_zero_successful_samples_distribution_is_rejected(self):
        """khive#945 item 1: a distribution present but with zero successful
        samples carries no measurement evidence."""
        record = _base_record()
        dist = dict(record["distributions"]["latency"])
        dist["successes"] = 0
        record["distributions"] = {"latency": dist}
        errors = flagship_schema.validate_record(record)
        self.assertTrue(any("successes" in e for e in errors), errors)

    def test_distribution_missing_error_accounting_fields_is_rejected(self):
        """khive#945 item 2/5: a distribution missing error/timeout
        accounting fields must fail schema validation."""
        record = _base_record()
        dist = dict(record["distributions"]["latency"])
        del dist["errors_by_code"]
        del dist["timed_out"]
        record["distributions"] = {"latency": dist}
        errors = flagship_schema.validate_record(record)
        self.assertTrue(any("errors_by_code" in e for e in errors), errors)
        self.assertTrue(any("timed_out" in e for e in errors), errors)

    def test_daemon_fallback_count_positive_is_rejected(self):
        """khive#945 item 3/5: a fallback-tainted row cannot count as
        measured until a positive daemon-engagement proof exists."""
        record = _base_record()
        record["runtime"]["daemon_fallback_count"] = 1
        errors = flagship_schema.validate_record(record)
        self.assertTrue(any("daemon_fallback_count" in e for e in errors), errors)

    def test_upgraded_base_record_fixture_still_passes(self):
        """khive#945 item 5: the upgraded positive fixture (real distribution
        evidence) must still be schema-valid."""
        errors = flagship_schema.validate_record(_base_record())
        self.assertEqual(errors, [])

    def test_missing_workload_manifest_metadata_is_flagged(self):
        record = _base_record()
        del record["workload"]["manifest_version"]
        del record["workload"]["manifest_hash"]
        errors = flagship_schema.validate_record(record)
        self.assertTrue(any("workload.manifest_version" in e for e in errors), errors)
        self.assertTrue(any("workload.manifest_hash" in e for e in errors), errors)

    def test_distribution_missing_errors_by_code_is_flagged(self):
        dist = {
            "estimator": "nearest_rank_v1",
            "unit": "us",
            "attempts": 1000,
            "successes": 1000,
            "timed_out": 0,
            "histogram_edges_us": [0, 1000],
            "histogram_counts": [500, 500],
            "p50_us": 900,
            "p95_us": 950,
            "p99_us": 990,
            "max_us": 1000,
            "conditional_on_success": True,
        }
        errors = flagship_schema.validate_distribution(dist)
        self.assertTrue(any("errors_by_code" in e for e in errors), errors)

    def test_record_rejects_unexpected_top_level_field(self):
        record = _base_record()
        record["unexpected_field"] = "surprise"
        errors = flagship_schema.validate_record(record)
        self.assertTrue(any("unexpected field(s)" in e for e in errors), errors)

    def test_missing_operation_or_arm_is_flagged(self):
        for field in ("operation", "arm"):
            record = _base_record()
            del record[field]
            errors = flagship_schema.validate_record(record)
            self.assertTrue(any(field in e for e in errors), errors)

    def test_arm_must_match_scenario_id_segment(self):
        record = _base_record(arm="cold_ann_overlap")
        errors = flagship_schema.validate_record(record)
        self.assertTrue(any("arm" in e for e in errors), errors)

    def test_six_segment_f3_scenario_id_with_wrong_arm_is_rejected(self):
        record = _base_record(
            scenario_id="f3.context.query.hop0.budget4k.real",
            feature="F3",
            operation="context",
            arm="entity_ids",
            workload={
                "manifest_version": "1",
                "manifest_hash": "sha256:" + "d" * 64,
                "scenario_id": "f3.context.query.hop0.budget4k.real",
                "fixture": "kg_context_fixture",
                "fixture_hash": "sha256:" + "a" * 64,
                "scale": {"entities": 1},
                "concurrency": 1,
                "attempts": 1000,
            },
        )
        errors = flagship_schema.validate_record(record)
        self.assertTrue(any("arm" in e for e in errors), errors)

    def test_scenario_id_with_fewer_than_3_segments_is_malformed(self):
        record = _base_record(scenario_id="f1.recall")
        errors = flagship_schema.validate_record(record)
        self.assertTrue(any(e.startswith("scenario_id:") for e in errors), errors)

    def test_errors_by_code_non_integer_value_is_flagged_not_raised(self):
        dist = {
            "estimator": "nearest_rank_v1",
            "unit": "us",
            "attempts": 10,
            "successes": 8,
            "timed_out": 0,
            "errors_by_code": {"E_TIMEOUT": "two"},
            "histogram_edges_us": [],
            "histogram_counts": [],
            "p50_us": None,
            "p95_us": None,
            "p99_us": None,
            "max_us": None,
            "conditional_on_success": True,
        }
        errors = flagship_schema.validate_distribution(dist)
        self.assertTrue(any("errors_by_code" in e for e in errors), errors)

    def test_errors_by_code_non_integer_value_through_validate_record(self):
        record = _base_record(
            distributions={
                "latency": {
                    "estimator": "nearest_rank_v1",
                    "unit": "us",
                    "attempts": 10,
                    "successes": 8,
                    "timed_out": 0,
                    "errors_by_code": {"E_TIMEOUT": "two"},
                    "histogram_edges_us": [],
                    "histogram_counts": [],
                    "p50_us": None,
                    "p95_us": None,
                    "p99_us": None,
                    "max_us": None,
                    "conditional_on_success": True,
                }
            }
        )
        errors = flagship_schema.validate_record(record)
        self.assertTrue(any("errors_by_code" in e for e in errors), errors)

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

    def test_artifact_sha256_none_is_rejected(self):
        """khive#945 M1: an absent digest value must not schema-validate."""
        record = _base_record(artifact={"name": "report.json", "sha256": None})
        errors = flagship_schema.validate_record(record)
        self.assertTrue(any("artifact.sha256" in e for e in errors), errors)

    def test_artifact_sha256_absent_key_is_rejected(self):
        record = _base_record(artifact={"name": "report.json"})
        errors = flagship_schema.validate_record(record)
        self.assertTrue(any("sha256" in e for e in errors), errors)

    def test_artifact_name_empty_is_rejected(self):
        record = _base_record(artifact={"name": "", "sha256": "b" * 64})
        errors = flagship_schema.validate_record(record)
        self.assertTrue(any("artifact.name" in e for e in errors), errors)

    def test_artifact_sha256_wrong_length_is_rejected(self):
        record = _base_record(artifact={"name": "report.json", "sha256": "b" * 63})
        errors = flagship_schema.validate_record(record)
        self.assertTrue(any("artifact.sha256" in e for e in errors), errors)

    def test_artifact_sha256_uppercase_is_rejected(self):
        record = _base_record(artifact={"name": "report.json", "sha256": "B" * 64})
        errors = flagship_schema.validate_record(record)
        self.assertTrue(any("artifact.sha256" in e for e in errors), errors)

    def test_error_status_record_with_empty_distributions_is_schema_valid(self):
        """khive#945 M3: an honest status='error' row with zero samples and a
        positive daemon_fallback_count must remain schema-valid - the
        measurement-evidence requirements are conditional on status=='ok'."""
        record = _base_record(status="error", distributions={})
        record["runtime"]["daemon_fallback_count"] = 3
        errors = flagship_schema.validate_record(record)
        self.assertEqual(errors, [])

    def test_ok_status_record_with_successes_99_is_rejected(self):
        """khive#945 item 4: the 100-success floor applies to status='ok'."""
        record = _base_record()
        dist = dict(record["distributions"]["latency"])
        dist["successes"] = 99
        record["distributions"] = {"latency": dist}
        errors = flagship_schema.validate_record(record)
        self.assertTrue(any("successes" in e and "100" in e for e in errors), errors)

    def test_ok_status_record_with_successes_100_is_accepted(self):
        record = _base_record()
        dist = dict(record["distributions"]["latency"])
        dist["successes"] = 100
        record["distributions"] = {"latency": dist}
        errors = flagship_schema.validate_record(record)
        self.assertEqual(errors, [])

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
        scenario = _base_scenario(runner_class="hosted_hash", embedder="bench_hash")
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
        scenario = _base_scenario(runner_class="hosted_hash", embedder="bench_hash")
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

    def test_hash_embedder_record_against_production_scenario_is_confounded(self):
        scenario = _base_scenario(embedder="production")
        record = _base_record(
            timestamp="2026-07-10T00:00:00+00:00",
            runtime={
                "surface": "mcp_daemon",
                "embedder": "bench_hash",
                "runner_class": "self_hosted_real_embedder",
                "daemon_fallback_count": 0,
            },
        )
        status, reason = coverage_validator.scenario_status(scenario, [record], self.NOW)
        self.assertEqual(status, "confounded")
        self.assertIn("embedder", reason)

    def test_attempts_1_record_against_1000_attempt_scenario_is_confounded(self):
        scenario = _base_scenario(attempts=1000)
        record = _base_record(
            timestamp="2026-07-10T00:00:00+00:00",
            workload={
                "manifest_version": "1",
                "manifest_hash": "sha256:" + "d" * 64,
                "scenario_id": "f1.recall.warm.real",
                "fixture": "memory_12k_sentinel_settled",
                "fixture_hash": "sha256:" + "a" * 64,
                "scale": {"memories": 12000},
                "concurrency": 1,
                "attempts": 1,
            },
        )
        status, reason = coverage_validator.scenario_status(scenario, [record], self.NOW)
        self.assertEqual(status, "confounded")
        self.assertIn("attempts", reason)

    def test_runner_class_mismatch_is_confounded(self):
        scenario = _base_scenario(runner_class="hosted_hash")
        record = _base_record(timestamp="2026-07-10T00:00:00+00:00")  # record stays self_hosted_real_embedder
        status, reason = coverage_validator.scenario_status(scenario, [record], self.NOW)
        self.assertEqual(status, "confounded")
        self.assertIn("runner_class", reason)

    def test_settle_state_mismatch_is_confounded(self):
        scenario = _base_scenario(state="cold")
        record = _base_record(timestamp="2026-07-10T00:00:00+00:00")  # record settle.state stays "warm"
        status, reason = coverage_validator.scenario_status(scenario, [record], self.NOW)
        self.assertEqual(status, "confounded")
        self.assertIn("settle.state", reason)

    def test_settle_method_mismatch_is_confounded(self):
        scenario = _base_scenario(settle="explicit_lifecycle_predicate")
        record = _base_record(timestamp="2026-07-10T00:00:00+00:00")  # record settle.method stays "sequential_sentinel"
        status, reason = coverage_validator.scenario_status(scenario, [record], self.NOW)
        self.assertEqual(status, "confounded")
        self.assertIn("settle.method", reason)

    def test_scale_mismatch_is_confounded(self):
        scenario = _base_scenario(scale={"memories": 50000})
        record = _base_record(timestamp="2026-07-10T00:00:00+00:00")  # record workload.scale stays {"memories": 12000}
        status, reason = coverage_validator.scenario_status(scenario, [record], self.NOW)
        self.assertEqual(status, "confounded")
        self.assertIn("scale", reason)

    def test_concurrency_mismatch_is_confounded(self):
        scenario = _base_scenario(concurrency=128)
        record = _base_record(timestamp="2026-07-10T00:00:00+00:00")  # record workload.concurrency stays 1
        status, reason = coverage_validator.scenario_status(scenario, [record], self.NOW)
        self.assertEqual(status, "confounded")
        self.assertIn("concurrency", reason)

    def test_operation_mismatch_is_confounded(self):
        scenario = _base_scenario(operation="memory.recall")
        record = _base_record(timestamp="2026-07-10T00:00:00+00:00", operation="memory.remember")
        status, reason = coverage_validator.scenario_status(scenario, [record], self.NOW)
        self.assertEqual(status, "confounded")
        self.assertIn("operation", reason)

    def test_workload_scenario_id_mismatch_is_confounded(self):
        scenario = _base_scenario()
        record = _base_record(
            timestamp="2026-07-10T00:00:00+00:00",
            workload={
                "manifest_version": "1",
                "manifest_hash": "sha256:" + "d" * 64,
                "scenario_id": "f1.recall.warm.hash",
                "fixture": "memory_12k_sentinel_settled",
                "fixture_hash": "sha256:" + "a" * 64,
                "scale": {"memories": 12000},
                "concurrency": 1,
                "attempts": 1000,
            },
        )
        status, reason = coverage_validator.scenario_status(scenario, [record], self.NOW)
        self.assertEqual(status, "confounded")
        self.assertIn("workload.scenario_id", reason)

    def test_errors_by_code_non_integer_value_is_confounded_via_scenario_status(self):
        scenario = _base_scenario()
        record = _base_record(
            timestamp="2026-07-10T00:00:00+00:00",
            distributions={
                "latency": {
                    "estimator": "nearest_rank_v1",
                    "unit": "us",
                    "attempts": 10,
                    "successes": 8,
                    "timed_out": 0,
                    "errors_by_code": {"E_TIMEOUT": "two"},
                    "histogram_edges_us": [],
                    "histogram_counts": [],
                    "p50_us": None,
                    "p95_us": None,
                    "p99_us": None,
                    "max_us": None,
                    "conditional_on_success": True,
                }
            },
        )
        status, _ = coverage_validator.scenario_status(scenario, [record], self.NOW)
        self.assertEqual(status, "confounded")

    def test_manifest_hash_mismatch_is_confounded_via_compute_coverage(self):
        scenario = _base_scenario()
        manifest = {"scenario": [scenario]}
        version, current_hash = coverage_validator.current_manifest_identity(manifest)
        self.assertEqual(version, "1")
        record = _base_record(
            timestamp="2026-07-10T00:00:00+00:00",
            workload={
                "manifest_version": version,
                "manifest_hash": "sha256:" + "e" * 64,  # differs from current_hash
                "scenario_id": "f1.recall.warm.real",
                "fixture": "memory_12k_sentinel_settled",
                "fixture_hash": "sha256:" + "a" * 64,
                "scale": {"memories": 12000},
                "concurrency": 1,
                "attempts": 1000,
            },
        )
        self.assertNotEqual(record["workload"]["manifest_hash"], current_hash)
        report = coverage_validator.compute_coverage(manifest, [record], self.NOW)
        self.assertEqual(report["counts"]["confounded"], 1)
        self.assertEqual(report["scenarios"][0]["status"], "confounded")
        self.assertIn("manifest_hash", report["scenarios"][0]["reason"])

    def test_manifest_version_mismatch_is_confounded_via_compute_coverage(self):
        scenario = _base_scenario()
        manifest = {"scenario": [scenario]}
        _, current_hash = coverage_validator.current_manifest_identity(manifest)
        record = _base_record(
            timestamp="2026-07-10T00:00:00+00:00",
            workload={
                "manifest_version": "0",  # stale manifest revision
                "manifest_hash": current_hash,
                "scenario_id": "f1.recall.warm.real",
                "fixture": "memory_12k_sentinel_settled",
                "fixture_hash": "sha256:" + "a" * 64,
                "scale": {"memories": 12000},
                "concurrency": 1,
                "attempts": 1000,
            },
        )
        report = coverage_validator.compute_coverage(manifest, [record], self.NOW)
        self.assertEqual(report["counts"]["confounded"], 1)
        self.assertIn("manifest_version", report["scenarios"][0]["reason"])

    def test_matching_manifest_identity_still_measured_via_compute_coverage(self):
        scenario = _base_scenario()
        manifest = {"scenario": [scenario]}
        version, current_hash = coverage_validator.current_manifest_identity(manifest)
        record = _base_record(
            timestamp="2026-07-10T00:00:00+00:00",
            workload={
                "manifest_version": version,
                "manifest_hash": current_hash,
                "scenario_id": "f1.recall.warm.real",
                "fixture": "memory_12k_sentinel_settled",
                "fixture_hash": "sha256:" + "a" * 64,
                "scale": {"memories": 12000},
                "concurrency": 1,
                "attempts": 1000,
            },
        )
        report = coverage_validator.compute_coverage(manifest, [record], self.NOW)
        self.assertEqual(report["counts"]["measured"], 1)

    def test_empty_distributions_row_no_longer_counts_as_measured(self):
        """khive#945 regression guard: the validator's former positive
        fixture shape (`distributions: {}`) must not be reported measured -
        it fails schema validation and the scenario is confounded."""
        scenario = _base_scenario()
        record = _base_record(timestamp="2026-07-10T00:00:00+00:00", distributions={})
        status, reason = coverage_validator.scenario_status(scenario, [record], self.NOW)
        self.assertEqual(status, "confounded")
        self.assertIn("distributions", reason)

    def test_fallback_tainted_row_no_longer_counts_as_measured(self):
        scenario = _base_scenario()
        record = _base_record(timestamp="2026-07-10T00:00:00+00:00")
        record["runtime"]["daemon_fallback_count"] = 3
        status, reason = coverage_validator.scenario_status(scenario, [record], self.NOW)
        self.assertEqual(status, "confounded")
        self.assertIn("daemon_fallback_count", reason)

    def test_missing_error_accounting_row_no_longer_counts_as_measured(self):
        scenario = _base_scenario()
        record = _base_record(timestamp="2026-07-10T00:00:00+00:00")
        dist = dict(record["distributions"]["latency"])
        del dist["errors_by_code"]
        record["distributions"] = {"latency": dist}
        status, reason = coverage_validator.scenario_status(scenario, [record], self.NOW)
        self.assertEqual(status, "confounded")

    def test_error_status_record_is_confounded_not_measured(self):
        """khive#945 M3: a structurally valid status='error' record with
        empty distributions and fallback evidence passes schema validation
        (see SchemaValidationTests) but must never be reported measured -
        coverage_validator confounds it via its own non-'ok' status."""
        scenario = _base_scenario()
        record = _base_record(status="error", distributions={}, timestamp="2026-07-10T00:00:00+00:00")
        record["runtime"]["daemon_fallback_count"] = 3
        self.assertEqual(flagship_schema.validate_record(record), [])
        status, reason = coverage_validator.scenario_status(scenario, [record], self.NOW)
        self.assertEqual(status, "confounded")
        self.assertIn("error", reason)

    def test_scenario_min_successes_floor_wins_over_lower_declared_minimum(self):
        """khive#945 item 4: a scenario cannot lower the 100-success floor -
        declaring min_successes=1 still fails a 99-success record."""
        scenario = _base_scenario(min_successes=1)
        record = _base_record(timestamp="2026-07-10T00:00:00+00:00")
        dist = dict(record["distributions"]["latency"])
        dist["successes"] = 99
        record["distributions"] = {"latency": dist}
        status, _ = coverage_validator.scenario_status(scenario, [record], self.NOW)
        self.assertEqual(status, "confounded")

    def test_scenario_min_successes_raises_required_minimum(self):
        """khive#945 item 4: a scenario declaring a minimum above the floor
        raises the bar - a 400-success record fails a 500-declared minimum."""
        scenario = _base_scenario(min_successes=500)
        record = _base_record(timestamp="2026-07-10T00:00:00+00:00")
        dist = dict(record["distributions"]["latency"])
        dist["successes"] = 400
        record["distributions"] = {"latency": dist}
        status, reason = coverage_validator.scenario_status(scenario, [record], self.NOW)
        self.assertEqual(status, "confounded")
        self.assertIn("min_successes", reason)

    def test_scenario_min_successes_above_floor_passes_when_met(self):
        scenario = _base_scenario(min_successes=500)
        record = _base_record(timestamp="2026-07-10T00:00:00+00:00")
        dist = dict(record["distributions"]["latency"])
        dist["successes"] = 500
        record["distributions"] = {"latency": dist}
        status, _ = coverage_validator.scenario_status(scenario, [record], self.NOW)
        self.assertEqual(status, "measured")

    def test_scenario_malformed_min_successes_confounds_instead_of_crashing(self):
        """khive#945 r2: a programmatic caller bypassing load_manifest with a
        malformed min_successes must get a confounded verdict, not a TypeError."""
        record = _base_record(timestamp="2026-07-10T00:00:00+00:00")
        for bad in ("500", True, 0, -5):
            scenario = _base_scenario(min_successes=bad)
            status, reason = coverage_validator.scenario_status(scenario, [record], self.NOW)
            self.assertEqual(status, "confounded", (bad, reason))
            self.assertIn("min_successes", reason)

    def test_artifact_sha256_none_is_confounded_not_measured_or_unverified(self):
        """khive#945 M1: an absent digest downgrades to confounded even when
        no --artifacts-dir is supplied (never measured/unverified)."""
        scenario = _base_scenario()
        record = _base_record(
            timestamp="2026-07-10T00:00:00+00:00", artifact={"name": "report.json", "sha256": None}
        )
        status, _ = coverage_validator.scenario_status(scenario, [record], self.NOW)
        self.assertEqual(status, "confounded")

    def test_artifact_sha256_absent_key_is_confounded_not_measured_or_unverified(self):
        scenario = _base_scenario()
        record = _base_record(timestamp="2026-07-10T00:00:00+00:00", artifact={"name": "report.json"})
        status, _ = coverage_validator.scenario_status(scenario, [record], self.NOW)
        self.assertEqual(status, "confounded")

    def test_latest_record_wins_when_multiple_exist(self):
        scenario = _base_scenario()
        older = _base_record(timestamp="2026-05-01T00:00:00+00:00")  # well outside 14d -> would be stale
        newer = _base_record(timestamp="2026-07-10T00:00:00+00:00")
        status, _ = coverage_validator.scenario_status(scenario, [older, newer], self.NOW)
        self.assertEqual(status, "measured")


class ArtifactVerificationTests(unittest.TestCase):
    """khive#945 item 4: raw artifact existence + sha256 verification when
    an --artifacts-dir is resolvable, and an explicit 'unverified' marker
    (never a silent pass) when it is not."""

    NOW = datetime.datetime(2026, 7, 11, tzinfo=datetime.timezone.utc)

    def _write_artifact(self, tmp: pathlib.Path, name: str, content: bytes) -> str:
        (tmp / name).write_bytes(content)
        return hashlib.sha256(content).hexdigest()

    def test_unverified_when_no_artifacts_dir_supplied(self):
        record = _base_record()
        verification, reason = coverage_validator.verify_artifact(record, None)
        self.assertEqual(verification, "unverified")
        self.assertIn("no --artifacts-dir", reason)

    def test_verified_when_file_exists_and_hash_matches(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = pathlib.Path(tmp)
            sha = self._write_artifact(tmp_path, "report.json", b"raw benchmark output")
            record = _base_record(artifact={"name": "report.json", "sha256": sha})
            verification, _ = coverage_validator.verify_artifact(record, tmp_path)
            self.assertEqual(verification, "verified")

    def test_missing_when_file_does_not_exist(self):
        with tempfile.TemporaryDirectory() as tmp:
            record = _base_record(artifact={"name": "report.json", "sha256": "b" * 64})
            verification, reason = coverage_validator.verify_artifact(record, pathlib.Path(tmp))
            self.assertEqual(verification, "missing")
            self.assertIn("not found", reason)

    def test_hash_mismatch_when_file_exists_but_hash_differs(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = pathlib.Path(tmp)
            self._write_artifact(tmp_path, "report.json", b"raw benchmark output")
            record = _base_record(artifact={"name": "report.json", "sha256": "b" * 64})
            verification, reason = coverage_validator.verify_artifact(record, tmp_path)
            self.assertEqual(verification, "hash_mismatch")
            self.assertIn("hashes to", reason)

    def test_compute_coverage_downgrades_measured_to_confounded_on_hash_mismatch(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = pathlib.Path(tmp)
            self._write_artifact(tmp_path, "report.json", b"raw benchmark output")
            scenario = _base_scenario()
            manifest = {"scenario": [scenario]}
            version, current_hash = coverage_validator.current_manifest_identity(manifest)
            record = _base_record(
                timestamp="2026-07-10T00:00:00+00:00",
                artifact={"name": "report.json", "sha256": "b" * 64},
                workload={
                    "manifest_version": version,
                    "manifest_hash": current_hash,
                    "scenario_id": "f1.recall.warm.real",
                    "fixture": "memory_12k_sentinel_settled",
                    "fixture_hash": "sha256:" + "a" * 64,
                    "scale": {"memories": 12000},
                    "concurrency": 1,
                    "attempts": 1000,
                },
            )
            report = coverage_validator.compute_coverage(manifest, [record], self.NOW, artifacts_dir=tmp_path)
            self.assertEqual(report["counts"]["confounded"], 1)
            self.assertEqual(report["scenarios"][0]["artifact_verification"], "hash_mismatch")

    def test_verify_artifact_rejects_none_sha256_even_without_artifacts_dir(self):
        """khive#945 M1: verify_artifact itself must check name/sha256
        validity before ever falling through to the 'unverified' state."""
        record = _base_record(artifact={"name": "report.json", "sha256": None})
        verification, reason = coverage_validator.verify_artifact(record, None)
        self.assertEqual(verification, "missing")
        self.assertIn("sha256", reason)

    def test_verify_artifact_rejects_absent_sha256_even_without_artifacts_dir(self):
        record = _base_record(artifact={"name": "report.json"})
        verification, reason = coverage_validator.verify_artifact(record, None)
        self.assertEqual(verification, "missing")
        self.assertIn("sha256", reason)

    def test_relative_traversal_name_is_missing_not_verified(self):
        """khive#945 M2: a '../' name must not resolve outside artifacts_dir."""
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = pathlib.Path(tmp)
            artifacts_dir = tmp_path / "artifacts"
            artifacts_dir.mkdir()
            sha = self._write_artifact(tmp_path, "outside.json", b"unrelated sibling file")
            record = _base_record(artifact={"name": "../outside.json", "sha256": sha})
            verification, reason = coverage_validator.verify_artifact(record, artifacts_dir)
            self.assertEqual(verification, "missing")
            self.assertIn("outside", reason)

    def test_absolute_name_is_missing_not_verified(self):
        """khive#945 M2: an absolute name escaping artifacts_dir must not verify."""
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = pathlib.Path(tmp)
            artifacts_dir = tmp_path / "artifacts"
            artifacts_dir.mkdir()
            sha = self._write_artifact(tmp_path, "outside.json", b"unrelated sibling file")
            absolute_name = str((tmp_path / "outside.json").resolve())
            record = _base_record(artifact={"name": absolute_name, "sha256": sha})
            verification, reason = coverage_validator.verify_artifact(record, artifacts_dir)
            self.assertEqual(verification, "missing")
            self.assertIn("outside", reason)

    def test_compute_coverage_reports_unverified_without_artifacts_dir(self):
        scenario = _base_scenario()
        manifest = {"scenario": [scenario]}
        version, current_hash = coverage_validator.current_manifest_identity(manifest)
        record = _base_record(
            timestamp="2026-07-10T00:00:00+00:00",
            workload={
                "manifest_version": version,
                "manifest_hash": current_hash,
                "scenario_id": "f1.recall.warm.real",
                "fixture": "memory_12k_sentinel_settled",
                "fixture_hash": "sha256:" + "a" * 64,
                "scale": {"memories": 12000},
                "concurrency": 1,
                "attempts": 1000,
            },
        )
        report = coverage_validator.compute_coverage(manifest, [record], self.NOW)
        self.assertEqual(report["counts"]["measured"], 1)
        self.assertEqual(report["scenarios"][0]["artifact_verification"], "unverified")


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

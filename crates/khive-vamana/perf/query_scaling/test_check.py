import copy
import hashlib
import json
import math
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import check


class ScalingEvidenceTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.path = self.root / "manifest.json"
        self.manifest = {
            "schema_version": 1, "source_commit": "a" * 40,
            "dataset": {"kind": "proxy", "name": "synthetic-test-only",
                        "model": "test", "model_revision": "test-v1", "snapshot": "fixture-v1",
                        "query_split": "fixed held-out fixture", "base_blake3": "b" * 64,
                        "query_blake3": "c" * 64},
            "runs": [],
        }
        self.runs = []
        for repeat in range(2):
            run = {
                "schema_version": "1.0", "git_sha": "a" * 40,
                "produced_at": f"2026-01-01T00:00:0{repeat}Z",
                "runner_os": "synthetic", "machine_model": "synthetic",
                "ram_bytes": 1024, "loadavg1": 0,
                "dataset": {"name": "synthetic-test-only", "base_blake3": "b" * 64,
                            "query_blake3": "c" * 64, "dim": 2, "base_n": 10000,
                            "query_n": 20, "normalization": "l2"},
                "config": {"k": 10, "target_recall": 0.95, "max_degree": 64,
                           "search_list_size": 128, "build_batch": 1024,
                           "max_iso_beam": 1024, "n_gt_queries": 20,
                           "n_latency_queries": 20, "alpha": 1.0},
                "gt_policy": {"source": "brute-force recomputed on subset"}, "rows": [],
                "fits": {"iso_recall_query_exponent_warm": 900},
                "decisive_question": {"verdict": "not authoritative"},
            }
            for n in [100, 300, 600, 1000]:
                run["rows"].append({"n": n, "recall_saturated": False, "recall_at_10": 1,
                                    "iso_recall_beam": 64, "gt_source": run["gt_policy"]["source"],
                                    "query_warm_p50_us": 10, "query_warm_p95_us": 11,
                                    "query_warm_p99_us": 12, "query_warm_max_us": 13,
                                    "bruteforce_p50_us": n / 10})
            self.runs.append(run)
        self.write()

    def write(self):
        self.manifest["runs"] = []
        for index, run in enumerate(self.runs):
            path = self.root / f"run-{index}.json"
            raw = (json.dumps(run) + "\n").encode()
            path.write_bytes(raw)
            self.manifest["runs"].append({"file": path.name, "sha256": hashlib.sha256(raw).hexdigest()})
        self.path.write_text(json.dumps(self.manifest))

    def test_flat_data_is_not_promoted_to_sublinear_growth(self):
        result = check.check(self.path)
        self.assertEqual(result["status"], "artifact_consistency_valid")
        self.assertEqual(result["n_span"], 10)
        self.assertEqual(result["shape_conclusion"], "requires interpretation of repeated measurements")
        for run in result["runs"]:
            self.assertAlmostEqual(run["fits"]["query"]["exponent"], 0)
            self.assertIsNone(run["fits"]["query"]["r_squared"])
            self.assertAlmostEqual(run["fits"]["brute_force"]["exponent"], 1)
            self.assertAlmostEqual(run["fits"]["brute_force"]["r_squared"], 1)

    def test_known_power_laws_recompute_fits_without_trusting_reported_verdict(self):
        for exponent in (0.3, 1, 1.2):
            with self.subTest(exponent=exponent):
                for run in self.runs:
                    for row in run["rows"]:
                        value = row["n"] ** exponent
                        for key in ("query_warm_p50_us", "query_warm_p95_us",
                                    "query_warm_p99_us", "query_warm_max_us"):
                            row[key] = value
                self.write()
                result = check.check(self.path)
                self.assertAlmostEqual(result["runs"][0]["fits"]["query"]["exponent"], exponent)
                self.assertAlmostEqual(result["runs"][0]["fits"]["query"]["r_squared"], 1)

    def test_bad_matrices_are_refused_even_with_good_reported_fit(self):
        for ns, expected in (([100, 500, 1000], "four distinct"),
                             ([100, 300, 300, 1000], "strictly increasing"),
                             ([100, 300, 600, 999], "10x"),
                             ([1000, 600, 300, 100], "strictly increasing")):
            with self.subTest(ns=ns):
                original = copy.deepcopy(self.runs)
                for run in self.runs:
                    run["rows"] = [dict(run["rows"][0], n=n) for n in ns]
                self.write()
                with self.assertRaisesRegex(check.EvidenceError, expected):
                    check.check(self.path)
                self.runs = original

    def test_run_corruption_is_refused(self):
        mutations = [
            (lambda r: r.update(git_sha="d" * 40), "commit mismatch"),
            (lambda r: r["dataset"].update(base_blake3="d" * 64), "base_blake3 mismatch"),
            (lambda r: r["dataset"].update(query_blake3="d" * 64), "query_blake3 mismatch"),
            (lambda r: r["dataset"].update(name="other"), "name mismatch"),
            (lambda r: r["config"].update(alpha=1.2), "separate cohorts"),
            (lambda r: r.update(machine_model="another host"), "separate cohorts"),
            (lambda r: r["rows"][0].update(recall_at_10=0.94), "recall_at_10"),
            (lambda r: r["rows"][0].update(recall_saturated=True), "saturated"),
            (lambda r: r["rows"][0].update(gt_source="full base"), "oracle mismatch"),
            (lambda r: r["rows"][0].update(query_warm_p50_us=math.nan), "invalid value"),
            (lambda r: r["rows"][0].update(bruteforce_p50_us=math.inf), "invalid value"),
            (lambda r: r["rows"][0].update(query_warm_p50_us=0), "invalid value"),
            (lambda r: r["rows"][0].update(query_warm_p50_us=14), "unordered"),
            (lambda r: r["rows"][0].update(n=True), "integer"),
            (lambda r: r["config"].update(target_recall=0.8), "target_recall"),
        ]
        for mutation, expected in mutations:
            with self.subTest(expected=expected):
                original = copy.deepcopy(self.runs)
                mutation(self.runs[1])
                self.write()
                with self.assertRaisesRegex(check.EvidenceError, expected):
                    check.check(self.path)
                self.runs = original

    def test_raw_archive_tamper_is_refused(self):
        path = self.root / "run-0.json"
        path.write_bytes(path.read_bytes() + b" ")
        with self.assertRaisesRegex(check.EvidenceError, "artifact hash mismatch"):
            check.check(self.path)

    def test_duplicate_or_single_run_is_refused(self):
        self.runs = [self.runs[0]]
        self.write()
        with self.assertRaisesRegex(check.EvidenceError, "two separate"):
            check.check(self.path)
        self.runs.append(copy.deepcopy(self.runs[0]))
        self.write()
        with self.assertRaisesRegex(check.EvidenceError, "duplicate run artifact"):
            check.check(self.path)
        self.runs[1]["loadavg1"] = 1
        self.write()
        with self.assertRaisesRegex(check.EvidenceError, "duplicate run timestamp"):
            check.check(self.path)

    def test_path_escape_and_duplicate_json_keys_are_refused(self):
        self.manifest["runs"][0]["file"] = "../outside.json"
        self.path.write_text(json.dumps(self.manifest))
        with self.assertRaisesRegex(check.EvidenceError, "local relative"):
            check.check(self.path)
        self.path.write_text('{"schema_version":1,"schema_version":1}')
        with self.assertRaisesRegex(check.EvidenceError, "duplicate JSON key"):
            check.check(self.path)

    def test_cli_has_machine_readable_success_and_nonzero_rejection(self):
        argv = [sys.executable, str(Path(check.__file__)), str(self.path)]
        result = subprocess.run(argv, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["status"], "artifact_consistency_valid")
        self.path.write_text("{}")
        result = subprocess.run(argv, capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)
        self.assertEqual(result.stdout, "")
        self.assertIn("invalid scaling evidence", result.stderr)


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
"""Exercise the feature guard with actual Cargo message shapes."""

import importlib.util
import json
import pathlib
import unittest

PATH = pathlib.Path(__file__).resolve().parents[1] / "check-audit-production-artifacts.py"
SPEC = importlib.util.spec_from_file_location("audit_artifacts", PATH)
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class AuditArtifactTests(unittest.TestCase):
    def artifact(self, features=(), test=False):
        return json.dumps({
            "reason": "compiler-artifact",
            "target": {"name": "khive_runtime", "kind": ["lib"]},
            "profile": {"test": test},
            "features": list(features),
        })

    def test_ordinary_and_cached_artifacts_are_accepted(self):
        MODULE.check_artifacts(["running 4 tests\n", self.artifact(), self.artifact()])

    def test_test_features_and_test_target_are_refused(self):
        for features, test in [(('test-internals',), False), (('fault-injection',), False), ((), True)]:
            with self.subTest(features=features, test=test), self.assertRaises(ValueError):
                MODULE.check_artifacts([self.artifact(features, test)])

    def test_missing_runtime_evidence_is_refused(self):
        with self.assertRaises(ValueError):
            MODULE.check_artifacts(['{"reason":"build-finished","success":true}\n'])
        event = json.loads(self.artifact())
        del event["features"]
        with self.assertRaises(ValueError):
            MODULE.check_artifacts([json.dumps(event)])


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
"""Regression tests for the local release SemVer gate helper."""

from __future__ import annotations

import importlib.util
import pathlib
import unittest


SCRIPT = pathlib.Path(__file__).resolve().parents[1] / "lib" / "semver_gate.py"
SPEC = importlib.util.spec_from_file_location("semver_gate", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
semver_gate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(semver_gate)


class CheckerVersionTests(unittest.TestCase):
    def test_rejects_checker_without_current_rustdoc_support(self):
        with self.assertRaisesRegex(ValueError, r"0\.48\.0.*>= 0\.50\.0"):
            semver_gate.validate_checker_version("cargo-semver-checks 0.48.0")

    def test_accepts_current_checker(self):
        self.assertEqual(
            semver_gate.validate_checker_version("cargo-semver-checks 0.50.0"),
            (0, 50, 0),
        )

    def test_rejects_unrecognized_version_output(self):
        with self.assertRaisesRegex(ValueError, "could not parse"):
            semver_gate.validate_checker_version("semver checker unknown")


class SemverSummaryTests(unittest.TestCase):
    def test_sums_evaluated_lints_across_success_and_finding_formats(self):
        log = """
        Checked [   0.100s] 3 checks: 3 pass, 251 skip
        Checked [   0.200s] 5 checks: 3 pass, 1 fail, 1 warn, 249 skip
        """
        self.assertEqual(semver_gate.summarize_log(log), (2, 8))

    def test_identifies_vacuous_pass_with_ansi_output(self):
        log = (
            "\x1b[32m     Checked\x1b[0m [   0.123s] 0 checks: 0 pass, 254 skip\n"
            "\x1b[32m     Checked\x1b[0m [   0.456s] 0 checks: 0 pass, 254 skip\n"
        )
        self.assertEqual(semver_gate.summarize_log(log), (2, 0))

    def test_fails_closed_when_checker_output_shape_is_unknown(self):
        with self.assertRaisesRegex(ValueError, "without any recognizable"):
            semver_gate.summarize_log("Summary no semver update required\n")


if __name__ == "__main__":
    unittest.main()

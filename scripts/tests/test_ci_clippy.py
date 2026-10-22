#!/usr/bin/env python3
"""Execute the CI Clippy phase against a recording Cargo process."""

from __future__ import annotations

import json
import os
import pathlib
import subprocess
import sys
import tempfile
import unittest

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]


class ClippyFeatureScopeTests(unittest.TestCase):
    def run_phase(self, scope=None, *, fail_mode="", metadata_error=False):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            cargo = root / "cargo"
            calls = root / "calls.jsonl"
            cargo.write_text(f"#!{sys.executable}\n" + r'''
import json, os, pathlib, sys
args = sys.argv[1:]
with pathlib.Path(os.environ["CARGO_CALL_LOG"]).open("a") as log:
    log.write(json.dumps(args) + "\n")
if args[0] == "metadata":
    if os.environ["METADATA_ERROR"] == "1":
        print("metadata unavailable", file=sys.stderr)
        sys.exit(17)
    print(json.dumps({"workspace_members": ["a", "b"], "packages": [
        {"id": "a", "name": "khive-db"}, {"id": "b", "name": "khive-runtime"}]}))
elif args[0] == "clippy":
    mode = "all" if "--all-features" in args else "default"
    if mode == os.environ["FAIL_MODE"]:
        sys.exit(43 if mode == "default" else 47)
else:
    raise AssertionError(args)
''')
            cargo.chmod(0o755)
            env = {
                **os.environ,
                "PATH": f"{root}{os.pathsep}{os.environ['PATH']}",
                "CARGO_CALL_LOG": str(calls),
                "FAIL_MODE": fail_mode,
                "METADATA_ERROR": str(int(metadata_error)),
            }
            env.pop("TEST_SCOPE", None)
            env.pop("CI_SH_TEST_FAIL_PHASE", None)
            if scope is not None:
                env["TEST_SCOPE"] = scope
            result = subprocess.run(
                ["sh", str(REPO_ROOT / "scripts" / "ci.sh"), "clippy"],
                env=env, text=True, capture_output=True, timeout=10,
            )
            commands = [json.loads(line) for line in calls.read_text().splitlines()]
            return result, commands

    def assert_feature_pair(self, calls, selection):
        self.assertEqual(calls, [
            ["clippy", *selection, "--all-targets", "--", "-D", "warnings"],
            ["clippy", *selection, "--all-targets", "--all-features", "--", "-D", "warnings"],
        ])

    def test_full_scope_checks_both_feature_sets(self):
        for scope in (None, "", "full"):
            with self.subTest(scope=scope):
                result, calls = self.run_phase(scope)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assert_feature_pair(calls, ["--workspace"])

    def test_scoped_checks_use_the_same_member_closure_and_exclude_external_crates(self):
        result, calls = self.run_phase("khive-db khive-merge khive-runtime")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls[0], ["metadata", "--no-deps", "--format-version", "1", "--locked"])
        self.assert_feature_pair(calls[1:], ["-p", "khive-db", "-p", "khive-runtime"])

    def test_no_selected_workspace_member_skips_both_checks(self):
        result, calls = self.run_phase("khive-merge")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([call[0] for call in calls], ["metadata"])
        self.assertIn("clippy skipped", result.stdout)

    def test_metadata_failure_does_not_fall_back_to_workspace(self):
        result, calls = self.run_phase("khive-db", metadata_error=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual([call[0] for call in calls], ["metadata"])
        self.assertIn("cannot read the workspace member list", result.stderr)

    def test_either_feature_failure_remains_a_failure(self):
        for scope in (None, "khive-db"):
            for mode, expected_rc, expected_calls in (("default", 43, 1), ("all", 47, 2)):
                with self.subTest(scope=scope, mode=mode):
                    result, calls = self.run_phase(scope, fail_mode=mode)
                    self.assertEqual(result.returncode, expected_rc, result.stderr)
                    self.assertEqual(sum(call[0] == "clippy" for call in calls), expected_calls)


class NoDefaultFeatureClippyTests(unittest.TestCase):
    FOLD_CLIPPY = [
        "clippy", "--locked", "-p", "khive-fold", "--no-default-features",
        "--lib", "--", "-D", "warnings",
    ]
    FOLD_TEST = [
        "test", "--locked", "-p", "khive-fold", "--no-default-features",
        "--test", "no_default_checkpoint",
    ]
    TYPES_CLIPPY = [
        "clippy", "--locked", "-p", "khive-types", "--no-default-features",
        "--all-targets", "--", "-D", "warnings",
    ]
    WORKSPACE_CLIPPY = [
        "clippy", "--workspace", "--all-targets", "--no-default-features",
        "--locked", "--", "-D", "warnings",
    ]

    def run_phase(self, scope=None, *, fail_call=0):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            cargo = root / "cargo"
            calls = root / "calls.jsonl"
            cargo.write_text(f"#!{sys.executable}\n" + r'''
import json, os, pathlib, sys
args = sys.argv[1:]
path = pathlib.Path(os.environ["CARGO_CALL_LOG"])
before = path.read_text().splitlines() if path.exists() else []
with path.open("a") as log:
    log.write(json.dumps(args) + "\n")
number = len(before) + 1
if number == int(os.environ["FAIL_CALL"]):
    print("injected Cargo failure", file=sys.stderr)
    sys.exit(40 + number)
''')
            cargo.chmod(0o755)
            env = {
                **os.environ,
                "PATH": f"{root}{os.pathsep}{os.environ['PATH']}",
                "CARGO_CALL_LOG": str(calls),
                "FAIL_CALL": str(fail_call),
            }
            env.pop("TEST_SCOPE", None)
            env.pop("CI_SH_TEST_FAIL_PHASE", None)
            if scope is not None:
                env["TEST_SCOPE"] = scope
            result = subprocess.run(
                ["sh", str(REPO_ROOT / "scripts" / "ci.sh"), "no-default-features"],
                env=env, text=True, capture_output=True, timeout=10,
            )
            commands = [json.loads(line) for line in calls.read_text().splitlines()]
            return result, commands

    def test_full_scope_lints_isolated_crates_and_workspace_without_defaults(self):
        expected = [
            self.FOLD_CLIPPY, self.FOLD_TEST, self.TYPES_CLIPPY, self.WORKSPACE_CLIPPY,
        ]
        for scope in (None, "", "full"):
            with self.subTest(scope=scope):
                result, calls = self.run_phase(scope)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(calls, expected)

    def test_scoped_isolated_legs_preserve_workspace_coverage(self):
        for scope, expected in (
            ("khive-fold", [self.FOLD_CLIPPY, self.FOLD_TEST]),
            ("khive-types", [self.TYPES_CLIPPY]),
            ("khive-types khive-fold", [self.FOLD_CLIPPY, self.FOLD_TEST, self.TYPES_CLIPPY]),
        ):
            with self.subTest(scope=scope):
                result, calls = self.run_phase(scope)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(calls, [*expected, self.WORKSPACE_CLIPPY])

    def test_unrelated_scope_skips_isolated_legs_only(self):
        result, calls = self.run_phase("khive-fold-extra khive-types-extra khive-merge")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls, [self.WORKSPACE_CLIPPY])
        self.assertIn("khive-fold is not under test", result.stdout)
        self.assertIn("khive-types is not under test", result.stdout)

    def test_each_cargo_failure_propagates_and_stops_the_phase(self):
        expected = [
            self.FOLD_CLIPPY, self.FOLD_TEST, self.TYPES_CLIPPY, self.WORKSPACE_CLIPPY,
        ]
        for number in range(1, len(expected) + 1):
            with self.subTest(failing_call=number):
                result, calls = self.run_phase(fail_call=number)
                self.assertEqual(result.returncode, 40 + number, result.stderr)
                self.assertEqual(calls, expected[:number])
                self.assertIn("injected Cargo failure", result.stderr)


if __name__ == "__main__":
    unittest.main()

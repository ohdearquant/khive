#!/usr/bin/env python3
"""Exercise dependency setup and test finality using owned, offline fake uv."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]


class ContractHarnessSetupTests(unittest.TestCase):
    def run_harness(self, setup_failures=0, test_status=0, command=None, phase=False):
        with tempfile.TemporaryDirectory(prefix="contract-setup-") as temporary:
            root = Path(temporary).resolve()
            (root / "scripts").mkdir()
            project = root / "tests/khive-contract"
            project.mkdir(parents=True)
            (root / "crates").mkdir()
            for name in ["ci-contract-harness.sh", "ci.sh"]:
                shutil.copy2(ROOT / "scripts" / name, root / "scripts" / name)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            # Fake both tools so tests never download, sleep, or start kkernel.
            fake = f"#!{sys.executable}\n" + """
import json, os, pathlib, sys
log = pathlib.Path(os.environ['HARNESS_FAKE_LOG'])
calls = json.loads(log.read_text()) if log.exists() else []
tool = pathlib.Path(sys.argv[0]).name
calls.append({'tool': tool, 'args': sys.argv[1:], 'cwd': os.getcwd()})
log.write_text(json.dumps(calls))
if tool == 'sleep':
    sys.exit(0)
if sys.argv[1] == 'sync':
    attempt = sum(c['tool'] == 'uv' and c['args'][0] == 'sync' for c in calls)
    if attempt <= int(os.environ['HARNESS_FAKE_SETUP_FAILURES']):
        print('Failed to download pluggy: operation timed out', file=sys.stderr)
        sys.exit(7)
    sys.exit(0)
assert sys.argv[1] == 'run'
if '--offline' not in sys.argv or '--no-sync' not in sys.argv:
    print('NETWORK_AFTER_SETUP_FORBIDDEN', file=sys.stderr)
    sys.exit(91)
status = int(os.environ['HARNESS_FAKE_TEST_STATUS'])
print('harness test failed' if status else 'harness tests passed')
sys.exit(status)
"""
            for name in ["uv", "sleep"]:
                executable = bin_dir / name
                executable.write_text(fake)
                executable.chmod(0o755)
            env = os.environ.copy()
            # The CI job exports its own TEST_SCOPE; a scope without kkernel would skip the contract-suite phase
            # under test and exit 0, so the harness runs unscoped whatever the calling job's scope is.
            env.pop("TEST_SCOPE", None)
            env.update(
                PATH=str(bin_dir) + os.pathsep + env.get("PATH", ""),
                HARNESS_FAKE_LOG=str(root / "calls.json"),
                HARNESS_FAKE_SETUP_FAILURES=str(setup_failures),
                HARNESS_FAKE_TEST_STATUS=str(test_status),
            )
            argv = command if command is not None else ["pytest", "-q"]
            if phase:
                argv = ["sh", str(root / "scripts/ci.sh"), *argv]
            else:
                argv = ["sh", str(root / "scripts/ci-contract-harness.sh"), *argv]
            result = subprocess.run(
                argv, env=env, capture_output=True, text=True, timeout=10, check=False
            )
            log = root / "calls.json"
            calls = json.loads(log.read_text()) if log.exists() else []
            # Capture cwd relative to the owned fixture before deleting it.
            for call in calls:
                call["cwd"] = str(Path(call["cwd"]).relative_to(root))
            return result, calls

    def test_success_runs_once_offline_without_syncing_during_tests(self):
        result, calls = self.run_harness(command=["pytest", "-q", "file with spaces.py"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([call["args"][0] for call in calls], ["sync", "run"])
        self.assertIn("--locked", calls[0]["args"])
        self.assertEqual(calls[1]["args"][-3:], ["pytest", "-q", "file with spaces.py"])
        self.assertTrue(all(call["cwd"] == "tests/khive-contract" for call in calls))
        self.assertIn("Contract Harness Execution", result.stdout)

    def test_transient_download_retries_only_setup_with_bounded_delays(self):
        result, calls = self.run_harness(setup_failures=2)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            [call["args"][0] for call in calls], ["sync", "2", "sync", "4", "sync", "run"]
        )

    def test_exhausted_download_reports_setup_failure_without_running_tests(self):
        result, calls = self.run_harness(setup_failures=100)
        self.assertEqual(result.returncode, 7)
        self.assertEqual(sum(call["args"][0] == "sync" for call in calls), 3)
        self.assertFalse(any(call["args"][0] == "run" for call in calls))
        self.assertIn("dependency download/environment setup failed", result.stderr)
        self.assertIn("no harness test was run", result.stderr)
        self.assertNotIn("Contract Harness Execution", result.stdout)

    def test_genuine_test_failure_is_not_retried_or_reported_as_setup_failure(self):
        result, calls = self.run_harness(test_status=5)
        self.assertEqual(result.returncode, 5)
        self.assertEqual([call["args"][0] for call in calls], ["sync", "run"])
        self.assertIn("harness test failed", result.stdout)
        self.assertNotIn("setup failed", result.stderr)

    def test_contract_suite_ci_phase_uses_the_same_setup_and_preserves_failure(self):
        result, calls = self.run_harness(test_status=4, command=["contract-suite"], phase=True)
        self.assertEqual(result.returncode, 4)
        self.assertEqual([call["args"][0] for call in calls], ["sync", "run"])
        self.assertEqual(calls[-1]["args"][-2:], ["pytest", "-q"])

    def test_empty_command_refuses_before_dependency_setup(self):
        result, calls = self.run_harness(command=[])
        self.assertEqual(result.returncode, 2)
        self.assertEqual(calls, [])


if __name__ == "__main__":
    unittest.main()

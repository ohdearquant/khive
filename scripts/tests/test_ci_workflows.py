#!/usr/bin/env python3
"""Contract tests for CI workflow triggers, permissions, and command wiring."""

from __future__ import annotations

import json
import os
import pathlib
import subprocess
import sys
import tempfile
import unittest

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
WORKFLOWS = REPO_ROOT / ".github" / "workflows"


def workflow_text(name: str) -> str:
    return (WORKFLOWS / name).read_text()


def indented_block(text: str, key: str, indent: int) -> str:
    lines = text.splitlines()
    marker = f"{' ' * indent}{key}:"
    start = lines.index(marker) + 1
    end = len(lines)
    for index in range(start, len(lines)):
        line = lines[index]
        if line.strip() and len(line) - len(line.lstrip()) <= indent:
            end = index
            break
    return "\n".join(lines[start:end])


def mapping_entries(block: str) -> set[str]:
    return {
        line.strip()
        for line in block.splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    }


class UnlockedDependencyWorkflowTests(unittest.TestCase):
    def test_weekly_workflow_uses_throwaway_lockfile_and_reports_all_outcomes(self):
        workflow = workflow_text("unlocked-dependencies.yml")
        triggers = indented_block(workflow, "on", 0)
        self.assertIn("schedule:", triggers)
        self.assertIn("workflow_dispatch:", triggers)
        self.assertNotIn("pull_request:", triggers)
        self.assertNotIn("push:", triggers)
        self.assertEqual(
            mapping_entries(indented_block(workflow, "permissions", 0)),
            {"contents: read"},
        )

        self.assertIn("$RUNNER_TEMP", workflow)
        self.assertIn("cargo update", workflow)
        self.assertIn("cargo check --workspace", workflow)
        self.assertIn("cargo test --workspace", workflow)
        self.assertIn("GITHUB_STEP_SUMMARY", workflow)


class CoverageRatchetWorkflowTests(unittest.TestCase):
    def test_measurement_job_reports_compute_unavailability(self):
        workflow = workflow_text("ci.yml")
        self.assertIn("  coverage-measurement:", workflow)
        measurement = indented_block(workflow, "coverage-measurement", 2)

        self.assertIn("name: Coverage measurement (advisory)", measurement)
        self.assertIn("id: compute_coverage", measurement)
        self.assertIn("continue-on-error: true", measurement)
        self.assertIn(
            "available: ${{ steps.compute_coverage.outcome == 'success' }}",
            measurement,
        )
        self.assertIn("if: steps.compute_coverage.outcome != 'success'", measurement)
        self.assertIn("Coverage measurement unavailable", measurement)
        self.assertIn("GITHUB_STEP_SUMMARY", measurement)

    def test_ratchet_requires_available_measurement_and_gate_tracks_both_jobs(self):
        workflow = workflow_text("ci.yml")
        measurement = indented_block(workflow, "coverage-measurement", 2)
        ratchet = indented_block(workflow, "coverage-ratchet", 2)
        gate = indented_block(workflow, "ci-gate", 2)

        self.assertIn("needs: coverage-measurement", ratchet)
        self.assertIn(
            "if: needs.coverage-measurement.outputs.available == 'true'", ratchet
        )
        self.assertNotIn("cargo llvm-cov", ratchet)
        self.assertIn("Check coverage does not regress", ratchet)
        self.assertIn("- coverage-measurement", gate)
        self.assertIn("- coverage-ratchet", gate)

        self.assertIn(
            "current: ${{ steps.compute_coverage.outputs.current }}", measurement
        )
        self.assertIn(
            "CURRENT_COVERAGE: ${{ needs.coverage-measurement.outputs.current }}",
            ratchet,
        )

    def test_measurement_reporting_step_is_best_effort(self):
        workflow = workflow_text("ci.yml")
        measurement = indented_block(workflow, "coverage-measurement", 2)
        report_step = measurement.split(
            "- name: Report unavailable coverage measurement", 1
        )[1]

        self.assertIn("continue-on-error: true", report_step)
        self.assertIn("Coverage measurement unavailable", report_step)


class AutoMergeGuardWorkflowTests(unittest.TestCase):
    def test_push_guard_has_only_required_write_permissions(self):
        workflow = workflow_text("ci.yml")
        guard = indented_block(workflow, "automerge-push-guard", 2)
        permissions = mapping_entries(indented_block(guard, "permissions", 4))
        self.assertEqual(permissions, {"contents: write", "pull-requests: write"})


class WasmtimeParityWorkflowTests(unittest.TestCase):
    def test_pinned_runtime_is_cached_retried_and_verified(self):
        workflow = workflow_text("ci.yml")
        job = indented_block(workflow, "wasm-parity", 2)

        self.assertIn("WASMTIME_VERSION: v46.0.1", job)
        self.assertIn("uses: actions/cache@v4", job)
        self.assertIn("id: cache-wasmtime", job)
        self.assertIn("path: ~/.wasmtime", job)
        self.assertIn(
            "key: wasmtime-${{ runner.os }}-${{ runner.arch }}-"
            "${{ env.WASMTIME_VERSION }}",
            job,
        )
        self.assertIn(
            "if: steps.cache-wasmtime.outputs.cache-hit != 'true'", job
        )
        self.assertIn("set -euo pipefail", job)
        self.assertIn("--retry 5", job)
        self.assertIn("--retry-all-errors", job)
        self.assertIn("releases/download/${WASMTIME_VERSION}", job)
        self.assertNotIn("wasmtime.dev/install.sh", job)
        self.assertIn('case "$RUNNER_ARCH" in', job)
        self.assertIn('X64) wasmtime_arch="x86_64" ;;', job)
        self.assertIn('ARM64) wasmtime_arch="aarch64" ;;', job)
        self.assertNotIn("x86_64-linux.tar.xz", job)

        verify_start = job.index("- name: Verify wasmtime version")
        verify_step = job[verify_start : job.index("- name:", verify_start + 1)]
        self.assertNotIn("if:", verify_step)
        self.assertIn('expected_version="${WASMTIME_VERSION#v}"', verify_step)
        self.assertIn('actual_version="${actual_version%% *}"', verify_step)
        self.assertIn(
            'if [[ "$actual_version" != "$expected_version" ]]; then', verify_step
        )
        self.assertNotIn('"wasmtime ${expected_version}"*', verify_step)
        self.assertIn('echo "$HOME/.wasmtime/bin" >> "$GITHUB_PATH"', verify_step)


class BenchTrackWorkflowTests(unittest.TestCase):
    def test_component_runner_limits_quick_flag_to_bench_targets(self):
        workflow = workflow_text("bench-component.yml")
        bench_commands = [
            line.strip() for line in workflow.splitlines() if "cargo bench" in line
        ]
        quick_commands = [line for line in bench_commands if "--quick" in line]
        self.assertEqual(len(quick_commands), 1)
        self.assertIn("--benches", quick_commands[0])
        self.assertIn("--criterion-dir crates/target/criterion", workflow)


class HarnessEnvironmentTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        tests_dir = str(REPO_ROOT / "tests")
        sys.path.insert(0, tests_dir)
        try:
            import kkernel_binary
            import smoke_test
        finally:
            sys.path.remove(tests_dir)
        cls.kkernel_binary = kkernel_binary
        cls.smoke = smoke_test

    def test_contract_binary_honors_cargo_target_dir_and_explicit_override(self):
        resolve = self.kkernel_binary.resolve_binary_path
        absolute_target = REPO_ROOT / ".test-target"
        self.assertEqual(
            pathlib.Path(resolve({"CARGO_TARGET_DIR": str(absolute_target)})),
            absolute_target / "release" / "kkernel",
        )
        self.assertEqual(
            pathlib.Path(resolve({"CARGO_TARGET_DIR": "custom-target"})),
            REPO_ROOT / "crates" / "custom-target" / "release" / "kkernel",
        )
        self.assertEqual(
            resolve(
                {
                    "CARGO_TARGET_DIR": str(absolute_target),
                    "KKERNEL_BINARY": "/explicit/kkernel",
                }
            ),
            "/explicit/kkernel",
        )

    def test_ci_sh_binary_resolver_matches_python_module(self):
        # ci.sh runs each phase as its own process on CI (release, contract-tests,
        # smoke-tests, vector-smoke, and contract-suite are separate workflow
        # steps), so the shell resolver and the Python resolver each phase's
        # harness uses must agree without relying on an inherited export.
        ci_sh = REPO_ROOT / "scripts" / "ci.sh"
        absolute_target = REPO_ROOT / ".test-target-abs"
        cases = [
            {"CARGO_TARGET_DIR": str(absolute_target)},
            {"CARGO_TARGET_DIR": "custom-target-rel"},
            {},
        ]
        for extra_env in cases:
            env = os.environ.copy()
            env.pop("KKERNEL_BINARY", None)
            env.pop("CARGO_TARGET_DIR", None)
            env.update(extra_env)
            completed = subprocess.run(
                ["sh", str(ci_sh), "--print-binary-path"],
                cwd=REPO_ROOT,
                env=env,
                check=True,
                capture_output=True,
                text=True,
            )
            shell_path = os.path.normpath(completed.stdout.strip())
            python_path = os.path.normpath(self.kkernel_binary.resolve_binary_path(env))
            self.assertEqual(shell_path, python_path, f"mismatch for env {extra_env}")

    def test_smoke_child_environment_removes_pack_override(self):
        child = self.smoke.smoke_child_env(
            {"KHIVE_PACKS": "kg,formal", "PRESERVED": "yes"}
        )
        self.assertNotIn("KHIVE_PACKS", child)
        self.assertEqual(child["PRESERVED"], "yes")
        self.assertEqual(child["KHIVE_NO_DAEMON"], "1")
        self.assertTrue(pathlib.Path(child["HOME"]).is_dir())
        self.assertEqual(
            self.smoke.DEFAULT_PACKS,
            {
                "kg",
                "gtd",
                "memory",
                "brain",
                "comm",
                "schedule",
                "knowledge",
                "session",
                "git",
                "code",
                "workspace",
                "blob",
            },
        )

    def test_full_ci_reports_failed_and_skipped_phases(self):
        # Run the real run_all loop with CI_SH_TEST_FAIL_PHASE forcing the first
        # phase to fail without executing it, and assert on the actual reported
        # output rather than on source text that could drift from behavior.
        ci_sh = REPO_ROOT / "scripts" / "ci.sh"
        env = os.environ.copy()
        env["CI_SH_TEST_FAIL_PHASE"] = "no-stubs-scan"
        completed = subprocess.run(
            ["sh", str(ci_sh)],
            cwd=REPO_ROOT,
            env=env,
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("Failed phase: no-stubs-scan (exit 1)", completed.stderr)
        self.assertIn("Skipped phases:", completed.stderr)
        self.assertIn("lockfile", completed.stderr)
        self.assertIn("contract-suite", completed.stderr)


class NpmReleaseWorkflowTests(unittest.TestCase):
    def test_release_publishes_cli_alias_after_exact_version_umbrella(self):
        workflow = workflow_text("release.yml")

        self.assertIn("ALIAS_VERSION=$(node -p", workflow)
        self.assertIn("ALIAS_KHIVE_VERSION=$(node -p", workflow)
        self.assertIn('if [ "$VERSION" != "$ALIAS_VERSION" ]', workflow)
        self.assertIn('if [ "$VERSION" != "$ALIAS_KHIVE_VERSION" ]', workflow)

        umbrella_publish = workflow.index("- name: Publish khive (umbrella)")
        alias_rewrite = workflow.index("- name: Set CLI alias version and khive dependency")
        alias_publish = workflow.index("- name: Publish @khive-ai/cli (compatibility alias)")
        self.assertLess(umbrella_publish, alias_rewrite)
        self.assertLess(alias_rewrite, alias_publish)
        self.assertIn("working-directory: npm/cli-alias", workflow[alias_publish:])

    def test_local_publish_dry_run_includes_cli_alias_after_umbrella(self):
        publish_script = REPO_ROOT / "scripts" / "npm-publish.sh"
        with tempfile.TemporaryDirectory() as temp_dir:
            npm_stub = pathlib.Path(temp_dir) / "npm"
            npm_stub.write_text(
                "#!/bin/sh\n"
                "if [ \"${1:-}\" = view ]; then echo 'npm ERR! code E404' >&2; exit 1; fi\n"
                "echo \"unexpected npm command: $*\" >&2\n"
                "exit 97\n"
            )
            npm_stub.chmod(0o755)
            env = os.environ.copy()
            env["PATH"] = f"{temp_dir}:{env['PATH']}"
            completed = subprocess.run(
                ["bash", str(publish_script), "--dry-run"],
                cwd=REPO_ROOT,
                env=env,
                check=False,
                capture_output=True,
                text=True,
            )

        self.assertEqual(completed.returncode, 0, completed.stderr)
        version = json.loads((REPO_ROOT / "npm" / "package.json").read_text())["version"]
        umbrella = f"[dry-run] would publish khive@{version}"
        alias = f"[dry-run] would publish @khive-ai/cli@{version}"
        self.assertIn(umbrella, completed.stdout)
        self.assertIn(alias, completed.stdout)
        self.assertLess(completed.stdout.index(umbrella), completed.stdout.index(alias))

    def test_local_publish_refuses_published_alias_with_wrong_khive_dependency(self):
        publish_script = REPO_ROOT / "scripts" / "npm-publish.sh"
        with tempfile.TemporaryDirectory() as temp_dir:
            npm_stub = pathlib.Path(temp_dir) / "npm"
            npm_stub.write_text(
                "#!/bin/sh\n"
                "if [ \"${1:-}\" = view ]; then\n"
                "  case \"${2:-}\" in @khive-ai/cli@*) echo 0.0.1; exit 0;; esac\n"
                "  exit 1\n"
                "fi\n"
                "echo \"unexpected npm command: $*\" >&2\n"
                "exit 97\n"
            )
            npm_stub.chmod(0o755)
            env = os.environ.copy()
            env["PATH"] = f"{temp_dir}:{env['PATH']}"
            completed = subprocess.run(
                ["bash", str(publish_script), "--dry-run"],
                cwd=REPO_ROOT,
                env=env,
                check=False,
                capture_output=True,
                text=True,
            )

        version = json.loads((REPO_ROOT / "npm" / "package.json").read_text())["version"]
        self.assertEqual(completed.returncode, 1, completed.stdout)
        self.assertIn(f"depends on khive 0.0.1, expected {version}", completed.stderr)
        self.assertNotIn("would publish @khive-ai/cli", completed.stdout)

    def test_local_publish_stops_when_the_alias_lookup_fails_for_another_reason(self):
        publish_script = REPO_ROOT / "scripts" / "npm-publish.sh"
        with tempfile.TemporaryDirectory() as temp_dir:
            npm_stub = pathlib.Path(temp_dir) / "npm"
            npm_stub.write_text(
                "#!/bin/sh\n"
                "if [ \"${1:-}\" = view ]; then\n"
                "  case \"${2:-}\" in @khive-ai/cli@*) echo 'npm ERR! code ECONNREFUSED' >&2; exit 1;; esac\n"
                "  echo 'npm ERR! code E404' >&2; exit 1\n"
                "fi\n"
                "echo \"unexpected npm command: $*\" >&2\n"
                "exit 97\n"
            )
            npm_stub.chmod(0o755)
            env = os.environ.copy()
            env["PATH"] = f"{temp_dir}:{env['PATH']}"
            completed = subprocess.run(
                ["bash", str(publish_script), "--dry-run"],
                cwd=REPO_ROOT,
                env=env,
                check=False,
                capture_output=True,
                text=True,
            )

        self.assertEqual(completed.returncode, 1, completed.stdout)
        self.assertIn("could not look up @khive-ai/cli@", completed.stderr)
        self.assertIn("ECONNREFUSED", completed.stderr)
        self.assertNotIn("would publish @khive-ai/cli", completed.stdout)


if __name__ == "__main__":
    unittest.main()

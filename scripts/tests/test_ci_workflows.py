#!/usr/bin/env python3
"""Contract tests for CI workflow triggers, permissions, and command wiring."""

from __future__ import annotations

import pathlib
import runpy
import sys
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
            cls.contract = runpy.run_path(str(REPO_ROOT / "tests" / "contract_test.py"))
            cls.smoke = runpy.run_path(str(REPO_ROOT / "tests" / "smoke_test.py"))
        finally:
            sys.path.remove(tests_dir)

    def test_contract_binary_honors_cargo_target_dir_and_explicit_override(self):
        resolve = self.contract["resolve_binary_path"]
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

    def test_smoke_child_environment_removes_pack_override(self):
        child = self.smoke["smoke_child_env"](
            {"KHIVE_PACKS": "kg,formal", "PRESERVED": "yes"}
        )
        self.assertNotIn("KHIVE_PACKS", child)
        self.assertEqual(child["PRESERVED"], "yes")
        self.assertEqual(child["KHIVE_NO_DAEMON"], "1")
        self.assertTrue(pathlib.Path(child["HOME"]).is_dir())
        self.assertEqual(
            self.smoke["DEFAULT_PACKS"],
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
        script = (REPO_ROOT / "scripts" / "ci.sh").read_text()
        self.assertIn("trap report_incomplete_ci 0", script)
        self.assertIn('echo "Failed phase: $current_phase (exit $status)"', script)
        self.assertIn('echo "Skipped phases: $skipped_phases"', script)

    def test_release_exports_one_binary_path_for_later_harnesses(self):
        script = (REPO_ROOT / "scripts" / "ci.sh").read_text()
        self.assertIn('cargo_target_dir=${CARGO_TARGET_DIR:-', script)
        self.assertIn('KKERNEL_BINARY="$cargo_target_dir/release/kkernel"', script)
        self.assertIn("export KKERNEL_BINARY", script)


if __name__ == "__main__":
    unittest.main()

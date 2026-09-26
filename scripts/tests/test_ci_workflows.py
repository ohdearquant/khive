#!/usr/bin/env python3
"""Contract tests for CI workflow triggers, permissions, and command wiring."""

from __future__ import annotations

import json
import os
import pathlib
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unittest

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
WORKFLOWS = REPO_ROOT / ".github" / "workflows"
REPLAY_NAME_PREFIX = "${{ github.event_name == 'workflow_dispatch' && inputs.main_sha != '' && 'Replay ' || '' }}"


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


def step_block(job, step_name):
    """The text of one `- name: <step_name>` step, up to the next step."""
    return job.split(f"- name: {step_name}", 1)[1].split("\n      - name: ", 1)[0]


class MinioStorageChangeWorkflowTests(unittest.TestCase):
    """Execute the shipped path gate with real, private shallow repositories."""

    def setUp(self):
        self.git = shutil.which("git")
        self.bash = shutil.which("bash")
        self.assertIsNotNone(self.git, "GIT_FIXTURE_TOOL_REQUIRED")
        self.assertIsNotNone(self.bash, "BASH_FIXTURE_TOOL_REQUIRED")

    def _gate_script(self):
        job = indented_block(workflow_text("ci.yml"), "minio-blob-compat", 2)
        step = step_block(job, "Determine whether storage crates changed")
        lines = step.splitlines()
        start = lines.index("        run: |") + 1
        script_lines = []
        for line in lines[start:]:
            if line.strip() and not line.startswith("          "):
                break
            script_lines.append(line)
        script = textwrap.dedent("\n".join(script_lines))
        self.assertTrue(script.strip(), "WORKFLOW_BASH_EXTRACTED")
        return script

    def _git(self, cwd, env, *args, check=True):
        result = subprocess.run(
            [self.git, *args],
            cwd=cwd,
            env=env,
            text=True,
            capture_output=True,
            timeout=30,
            check=False,
        )
        if check:
            self.assertEqual(
                result.returncode,
                0,
                f"GIT_FIXTURE_SETUP: {args!r}\n{result.stdout}\n{result.stderr}",
            )
        return result

    def _shallow_fixture(self, changed_path="README.md"):
        temporary = tempfile.TemporaryDirectory(prefix="minio-workflow-")
        self.addCleanup(temporary.cleanup)
        root = pathlib.Path(temporary.name)
        home = root / "home"
        hooks = root / "empty-hooks"
        home.mkdir()
        hooks.mkdir()
        # No inherited Git config, signing, hooks, credentials, or shell startup
        # files may affect this fixture. Only its private file:// remote is used.
        env = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith(("GIT_", "BASH_FUNC_")) and key not in {"BASH_ENV", "ENV"}
        }
        env.update(
            HOME=str(home),
            XDG_CONFIG_HOME=str(home / "config"),
            GIT_CONFIG_NOSYSTEM="1",
            GIT_CONFIG_SYSTEM=os.devnull,
            GIT_CONFIG_GLOBAL=os.devnull,
            GIT_TERMINAL_PROMPT="0",
            GIT_ALLOW_PROTOCOL="file",
        )
        config = {
            "user.name": "MinIO workflow fixture",
            "user.email": "minio-workflow@example.invalid",
            "commit.gpgsign": "false",
            "tag.gpgsign": "false",
            "core.hooksPath": str(hooks),
            "core.autocrlf": "false",
            "core.quotePath": "true",
        }
        env["GIT_CONFIG_COUNT"] = str(len(config))
        for index, (key, value) in enumerate(config.items()):
            env[f"GIT_CONFIG_KEY_{index}"] = key
            env[f"GIT_CONFIG_VALUE_{index}"] = value

        source = root / "source"
        remote = root / "remote.git"
        checkout = root / "checkout"
        self._git(root, env, "init", "--bare", str(remote))
        self._git(root, env, "init", "--initial-branch=main", str(source))
        (source / "crates").mkdir()
        (source / "crates" / ".keep").write_text("fixture working directory\n")
        (source / "README.md").write_text("base revision\n")
        self._git(source, env, "add", "--all")
        self._git(source, env, "commit", "-m", "base fixture")
        base = self._git(source, env, "rev-parse", "HEAD").stdout.strip()
        changed = source / changed_path
        changed.parent.mkdir(parents=True, exist_ok=True)
        changed.write_text("changed revision\n")
        self._git(source, env, "add", "--all")
        self._git(source, env, "commit", "-m", "change fixture")
        head = self._git(source, env, "rev-parse", "HEAD").stdout.strip()
        remote_url = remote.as_uri()
        self._git(source, env, "push", remote_url, "HEAD:refs/heads/main")
        self._git(
            root, env, "clone", "--depth=1", "--no-local", "--branch", "main",
            remote_url, str(checkout),
        )
        self.assertEqual(
            self._git(checkout, env, "rev-parse", "HEAD").stdout.strip(),
            head,
            "SHALLOW_CHECKOUT_IS_CHANGED_REVISION",
        )
        self.assertEqual(
            self._git(checkout, env, "rev-parse", "--is-shallow-repository").stdout.strip(),
            "true",
            "FIXTURE_IS_DEPTH_ONE_CLONE",
        )
        self.assertNotEqual(
            self._git(checkout, env, "cat-file", "-e", f"{base}^{{commit}}", check=False).returncode,
            0,
            "SHALLOW_PARENT_INITIALLY_ABSENT",
        )
        return checkout, base, env

    def _run_gate(self, checkout, env, *, event="pull_request", pr_base="", push_before=""):
        output = checkout.parent / "github-output"
        gate_env = {
            **env,
            "EVENT_NAME": event,
            "PR_BASE_SHA": pr_base,
            "PUSH_BEFORE_SHA": push_before,
            "GITHUB_OUTPUT": str(output),
        }
        result = subprocess.run(
            [self.bash, "--noprofile", "--norc", "-c", self._gate_script()],
            cwd=checkout / "crates",
            env=gate_env,
            text=True,
            capture_output=True,
            timeout=30,
            check=False,
        )
        self.assertEqual(
            result.returncode,
            0,
            f"WORKFLOW_BASH_SUCCEEDED\n{result.stdout}\n{result.stderr}",
        )
        self.assertTrue(output.is_file(), "WORKFLOW_OUTPUT_WAS_WRITTEN")
        return output.read_text()

    def _assert_base_fetched(self, checkout, base, env):
        self.assertEqual(
            self._git(checkout, env, "cat-file", "-e", f"{base}^{{commit}}", check=False).returncode,
            0,
            "COMPARISON_BASE_WAS_FETCHED",
        )

    def test_pull_request_nonstorage_change_fetches_missing_base(self):
        checkout, base, env = self._shallow_fixture()
        self.assertEqual(
            self._run_gate(checkout, env, pr_base=base),
            "run=false\n",
            "PR_BASE_FETCH_SKIPS_NONSTORAGE",
        )
        self._assert_base_fetched(checkout, base, env)

    def test_push_nonstorage_change_fetches_missing_base(self):
        checkout, base, env = self._shallow_fixture()
        self.assertEqual(
            self._run_gate(checkout, env, event="push", push_before=base),
            "run=false\n",
            "PUSH_BASE_FETCH_SKIPS_NONSTORAGE",
        )
        self._assert_base_fetched(checkout, base, env)

    def _assert_protected_path(self, path, marker):
        checkout, base, env = self._shallow_fixture(path)
        self.assertEqual(
            self._run_gate(checkout, env, pr_base=base), "run=true\n", marker,
        )
        self._assert_base_fetched(checkout, base, env)

    def test_database_path_runs_minio_after_fetching_base(self):
        self._assert_protected_path("crates/khive-db/src/fixture.rs", "DATABASE_PATH_RUNS_MINIO")

    def _assert_quoted_database_path(self, path, marker):
        checkout, base, env = self._shallow_fixture(path)
        self.assertEqual(self._run_gate(checkout, env, pr_base=base), "run=true\n", marker)
        self._assert_base_fetched(checkout, base, env)
        listed = self._git(checkout, env, "diff", "--name-only", base, "HEAD").stdout
        self.assertTrue(
            listed.startswith('"'),
            f"{marker}: fixture must expose Git's C-quoted name: {listed!r}",
        )

    def test_nonascii_database_path_runs_minio_after_fetching_base(self):
        self._assert_quoted_database_path(
            "crates/khive-db/tests/é.rs", "NONASCII_DATABASE_PATH_RUNS_MINIO",
        )

    def test_tab_in_database_path_runs_minio_after_fetching_base(self):
        self._assert_quoted_database_path(
            "crates/khive-db/tests/a\tb.rs", "TAB_DATABASE_PATH_RUNS_MINIO",
        )

    def test_storage_path_runs_minio_after_fetching_base(self):
        self._assert_protected_path("crates/khive-storage/src/fixture.rs", "STORAGE_PATH_RUNS_MINIO")

    def test_blob_contract_path_runs_minio_after_fetching_base(self):
        self._assert_protected_path("docs/adr/ADR-111-fixture.md", "BLOB_CONTRACT_PATH_RUNS_MINIO")

    def test_ci_workflow_path_runs_minio_after_fetching_base(self):
        self._assert_protected_path(".github/workflows/ci.yml", "CI_WORKFLOW_PATH_RUNS_MINIO")

    def test_workspace_manifest_path_runs_minio_after_fetching_base(self):
        self._assert_protected_path("crates/Cargo.toml", "WORKSPACE_MANIFEST_PATH_RUNS_MINIO")

    def test_workspace_lockfile_path_runs_minio_after_fetching_base(self):
        self._assert_protected_path("crates/Cargo.lock", "WORKSPACE_LOCKFILE_PATH_RUNS_MINIO")

    def test_renaming_database_conformance_to_unprotected_crate_runs_minio(self):
        protected = "crates/khive-db/tests/blob_conformance.rs"
        unprotected = "crates/khive-types/tests/blob_conformance.rs"
        checkout, _, env = self._shallow_fixture(protected)
        base = self._git(checkout, env, "rev-parse", "HEAD").stdout.strip()
        (checkout / unprotected).parent.mkdir(parents=True)
        self._git(checkout, env, "mv", protected, unprotected)
        self._git(checkout, env, "commit", "-m", "move conformance out of database crate")
        # Pin the fixture to the behavior that hid the protected old path.
        # The shipped script must override this with --no-renames.
        self._git(checkout, env, "config", "diff.renames", "true")
        self.assertEqual(
            self._git(checkout, env, "diff", "--name-status", base, "HEAD").stdout,
            f"R100\t{protected}\t{unprotected}\n",
            "FIXTURE_IS_PROTECTED_TO_UNPROTECTED_RENAME",
        )
        self.assertEqual(
            self._git(checkout, env, "diff", "--name-only", base, "HEAD").stdout,
            f"{unprotected}\n",
            "DEFAULT_RENAME_DIFF_HIDES_PROTECTED_SOURCE",
        )
        self.assertEqual(
            self._run_gate(checkout, env, pr_base=base),
            "run=true\n",
            "RENAMED_DATABASE_SOURCE_RUNS_MINIO",
        )

    def test_missing_base_runs_minio(self):
        checkout, _, env = self._shallow_fixture()
        self.assertEqual(
            self._run_gate(checkout, env), "run=true\n", "MISSING_BASE_RUNS_MINIO",
        )

    def test_new_branch_zero_push_base_runs_minio(self):
        checkout, _, env = self._shallow_fixture()
        self.assertEqual(
            self._run_gate(checkout, env, event="push", push_before="0" * 40),
            "run=true\n",
            "ZERO_PUSH_BASE_RUNS_MINIO",
        )

    def test_unresolvable_base_runs_minio(self):
        checkout, _, env = self._shallow_fixture()
        self.assertEqual(
            self._run_gate(checkout, env, pr_base="f" * 40),
            "run=true\n",
            "UNRESOLVABLE_BASE_RUNS_MINIO",
        )

    def test_non_sha_local_ref_runs_minio(self):
        checkout, _, env = self._shallow_fixture()
        self.assertEqual(
            self._run_gate(checkout, env, pr_base="HEAD"),
            "run=true\n",
            "NON_SHA_BASE_RUNS_MINIO",
        )

    def test_manual_event_runs_minio_even_with_comparable_payload_bases(self):
        checkout, base, env = self._shallow_fixture()
        self._git(checkout, env, "fetch", "--no-tags", "--depth=1", "origin", base)
        self.assertEqual(
            self._run_gate(
                checkout, env, event="workflow_dispatch", pr_base=base, push_before=base,
            ),
            "run=true\n",
            "MANUAL_EVENT_RUNS_MINIO",
        )

    def test_scheduled_event_runs_minio_even_with_comparable_payload_bases(self):
        checkout, base, env = self._shallow_fixture()
        self._git(checkout, env, "fetch", "--no-tags", "--depth=1", "origin", base)
        self.assertEqual(
            self._run_gate(
                checkout, env, event="schedule", pr_base=base, push_before=base,
            ),
            "run=true\n",
            "SCHEDULED_EVENT_RUNS_MINIO",
        )

    def test_locally_present_base_skips_nonstorage_without_remote(self):
        checkout, base, env = self._shallow_fixture()
        self._git(checkout, env, "fetch", "--no-tags", "--depth=1", "origin", base)
        self._git(checkout, env, "remote", "remove", "origin")
        self.assertEqual(
            self._run_gate(checkout, env, pr_base=base),
            "run=false\n",
            "LOCAL_BASE_NEEDS_NO_REMOTE",
        )

    def test_diff_failure_runs_minio(self):
        checkout, base, env = self._shallow_fixture()
        wrappers = checkout.parent / "wrappers"
        wrappers.mkdir()
        trace = checkout.parent / "diff-failure-trace"
        wrapper = wrappers / "git"
        wrapper.write_text(
            "#!/bin/sh\n"
            'if [ "$1" = "diff" ]; then\n'
            '  printf "%s\\n" "forced diff failure" >> "$CI_DIFF_FAILURE_TRACE"\n'
            "  exit 73\n"
            "fi\n"
            f"exec {shlex.quote(self.git)} \"$@\"\n"
        )
        wrapper.chmod(0o700)
        gate_env = {
            **env,
            "PATH": f"{wrappers}{os.pathsep}{env.get('PATH', os.defpath)}",
            "CI_DIFF_FAILURE_TRACE": str(trace),
        }
        self.assertEqual(
            self._run_gate(checkout, gate_env, pr_base=base),
            "run=true\n",
            "DIFF_FAILURE_RUNS_MINIO",
        )
        self.assertTrue(trace.is_file(), "DIFF_FAILURE_WAS_EXERCISED")
        self.assertEqual(trace.read_text(), "forced diff failure\n", "ONLY_DIFF_WAS_FORCED_TO_FAIL")
        self._assert_base_fetched(checkout, base, env)


class CoverageRatchetWorkflowTests(unittest.TestCase):
    def test_measurement_job_reports_compute_unavailability(self):
        workflow = workflow_text("ci.yml")
        self.assertIn("  coverage-measurement:", workflow)
        measurement = indented_block(workflow, "coverage-measurement", 2)

        # The job was named "(advisory)" while a missing measurement left the
        # gate green. It is not advisory now: the step below turns an absent
        # measurement into a failing job, so the name would misdescribe it.
        self.assertIn("name: " + REPLAY_NAME_PREFIX + "Coverage measurement\n", measurement)
        self.assertNotIn("advisory", measurement)
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

        self.assertIn("needs: [resolve-revision, coverage-measurement]", ratchet)
        self.assertIn(
            "if: needs.resolve-revision.result == 'success' && "
            "needs.coverage-measurement.result == 'success' && "
            "needs.coverage-measurement.outputs.available == 'true'",
            step_block(ratchet, "Check coverage does not regress"),
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
        measurement = indented_block(workflow_text("ci.yml"), "coverage-measurement", 2)
        # Bounded to this step. Unbounded, the tail of the job satisfied the
        # continue-on-error assertion from whatever step came next, and a step
        # that must NOT be best-effort now follows this one.
        report_step = step_block(measurement, "Report unavailable coverage measurement")

        self.assertIn("continue-on-error: true", report_step)
        self.assertIn("Coverage measurement unavailable", report_step)

    def test_absent_measurement_fails_the_job(self):
        measurement = indented_block(workflow_text("ci.yml"), "coverage-measurement", 2)
        fail_step = step_block(measurement, "Fail when no measurement was produced")

        self.assertIn("if: steps.compute_coverage.outcome != 'success'", fail_step)
        self.assertNotIn("continue-on-error", fail_step)
        self.assertIn("exit 1", fail_step)

    def test_measuring_budget_stays_inside_the_job_budget(self):
        measurement = indented_block(workflow_text("ci.yml"), "coverage-measurement", 2)
        job = re.findall(r"(?m)^    timeout-minutes: (\d+)$", measurement)
        step = re.findall(r"(?m)^        timeout-minutes: (\d+)$", measurement)
        self.assertEqual((len(job), len(step)), (1, 1))
        # A job-level timeout cancels the job, and a cancelled job is neither
        # success nor skipped, so the step budget has to expire first for the
        # reporting and failing steps above to run at all.
        self.assertLess(int(step[0]), int(job[0]))


class AutoMergeGuardWorkflowTests(unittest.TestCase):
    def test_push_guard_has_only_required_write_permissions(self):
        workflow = workflow_text("ci.yml")
        guard = indented_block(workflow, "automerge-push-guard", 2)
        permissions = mapping_entries(indented_block(guard, "permissions", 4))
        self.assertEqual(permissions, {"contents: write", "pull-requests: write"})


class AggregateGateWorkflowTests(unittest.TestCase):
    # The push guard is the only remaining job with an eligibility predicate.
    # always() admits a job after failed dependencies; it is not such a predicate.
    CONDITIONAL_JOBS = {"automerge-push-guard"}
    FORGIVEN_SKIPS = {"automerge-push-guard"}

    def setUp(self):
        self.workflow = workflow_text("ci.yml")
        self.gate = indented_block(self.workflow, "ci-gate", 2)
        self.needs = {
            line.strip().removeprefix("- ")
            for line in indented_block(self.gate, "needs", 4).splitlines()
        }
        step = self.gate.split("- name: Check aggregated job results\n", 1)[1]
        self.script = textwrap.dedent(step.split("        run: |\n", 1)[1])

    def run_gate(self, results):
        return subprocess.run(
            ["bash", "-e", "-c", self.script],
            env={**os.environ, "NEEDS": json.dumps({
                job: {"result": result} for job, result in results.items()
            })},
            capture_output=True, text=True, timeout=5, check=False,
        )

    def test_gate_forgives_skips_only_for_jobs_on_its_allow_list(self):
        conditional = {
            job for job in self.needs
            if re.search(r"(?m)^    if:", indented_block(self.workflow, job, 2))
            and "    if: always()" not in indented_block(self.workflow, job, 2)
        }
        self.assertEqual(conditional, self.CONDITIONAL_JOBS)
        self.assertTrue(self.needs - conditional)
        self.assertEqual(self.FORGIVEN_SKIPS, conditional)
        for job in sorted(self.needs):
            for outcome in ("success", "skipped", "failure", "cancelled"):
                with self.subTest(job=job, outcome=outcome):
                    results = dict.fromkeys(self.needs, "success")
                    results[job] = outcome
                    result = self.run_gate(results)
                    accepted = outcome == "success" or (
                        outcome == "skipped" and job in self.FORGIVEN_SKIPS
                    )
                    self.assertEqual(
                        result.returncode, 0 if accepted else 1,
                        f"AGGREGATE_SKIP_POLICY: {job}={outcome}\n"
                        + result.stdout + result.stderr,
                    )
                    if not accepted:
                        self.assertIn(f"Gate failure — jobs not green: {job}", result.stdout)

    def test_gate_new_dependency_does_not_inherit_skip_exemption(self):
        for outcome in ("success", "skipped", "failure", "cancelled"):
            with self.subTest(outcome=outcome):
                results = dict.fromkeys(self.needs, "success")
                results["new-required-check"] = outcome
                result = self.run_gate(results)
                self.assertEqual(result.returncode, 0 if outcome == "success" else 1,
                                 result.stdout + result.stderr)

    def test_gate_mixed_results_report_only_rejected_jobs(self):
        # Only the push guard's skip is expected; the newly admitted jobs must
        # finish their no-work/failure path instead of silently skipping.
        results = {
            "ci": "skipped", "docs": "failure", "secret-scan": "cancelled",
            "automerge-push-guard": "skipped", "dependency-review": "skipped",
            "coverage-ratchet": "skipped",
        }
        result = self.run_gate(results)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertEqual(
            result.stdout.split("Gate failure — jobs not green: ", 1)[1].splitlines(),
            ["ci", "docs", "secret-scan", "dependency-review", "coverage-ratchet"],
        )


class CiConcurrencyWorkflowTests(unittest.TestCase):
    def test_main_pushes_run_newest_commit_without_cancelling_running_run(self):
        concurrency = indented_block(workflow_text("ci.yml"), "concurrency", 0)
        self.assertEqual(
            mapping_entries(concurrency),
            {
                "group: ci-${{ github.workflow }}-${{ "
                "github.event_name == 'pull_request' && github.ref || "
                "github.event_name == 'push' && github.ref == 'refs/heads/main' "
                "&& github.ref || github.run_id }}",
                "cancel-in-progress: ${{ github.event_name == 'pull_request' }}",
            },
            "Main pushes must share one group whose running run is never "
            "cancelled and whose waiting run is replaced by the newest push "
            "(the default single-slot queue); PRs retain ref-scoped "
            "cancellation, and other events retain independent run groups.",
        )


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
        self.assertIn("if: steps.cache-wasmtime.outputs.cache-hit != 'true'", job)
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
    def test_component_phases_bound_children_and_report_raw_exit_codes(self):
        workflow = workflow_text("bench-component.yml")
        for phase, name, limit in [
            ("compile", "Compile-check this component's bench targets", "1500"),
            (
                "criterion",
                "Run Criterion benches (quick profile, bounded to 10 minutes)",
                "600",
            ),
        ]:
            step = workflow.split(f"      - name: {name}\n", 1)[1].split(
                "      - name:", 1
            )[0]
            script = textwrap.dedent(step.split("        run: |\n", 1)[1])
            for rc in (0, 7, 124, 137):
                with (
                    self.subTest(phase=phase, rc=rc),
                    tempfile.TemporaryDirectory() as tmp,
                ):
                    root = pathlib.Path(tmp)
                    work = root / "crates"
                    work.mkdir()
                    bins = root / "bin"
                    bins.mkdir()
                    timeout = bins / "timeout"
                    timeout.write_text(
                        '#!/bin/sh\nprintf "%s\\n" "$@" > "$ARGV_LOG"\nexit "$FAKE_RC"\n'
                    )
                    timeout.chmod(0o755)
                    output = root / "outputs"
                    args = root / "args"
                    env = {
                        **os.environ,
                        "PATH": f"{bins}:{os.environ['PATH']}",
                        "COMPONENT_CRATES": "khive-pack-gtd khive-pack-kg",
                        "COMPONENT_JOB_MINUTES": "40",
                        "GITHUB_OUTPUT": str(output),
                        "ARGV_LOG": str(args),
                        "FAKE_RC": str(rc),
                    }
                    result = subprocess.run(
                        ["bash", "-c", script],
                        cwd=work,
                        env=env,
                        capture_output=True,
                        text=True,
                        timeout=5,
                        check=False,
                    )
                    self.assertEqual(
                        result.returncode,
                        rc if phase == "compile" else 0,
                        result.stderr,
                    )
                    argv = args.read_text().splitlines()
                    self.assertEqual(
                        argv[:4], ["--kill-after=30s", limit, "cargo", "bench"]
                    )
                    self.assertIn("--benches", argv)
                    self.assertNotIn("--all-targets", argv)
                    log = (root / "component-phases.log").read_text()
                    self.assertIn(f"phase={phase} start=", log)
                    self.assertRegex(
                        log, rf"phase={phase} elapsed_seconds=\d+ exit_code={rc}"
                    )
                    if rc:
                        self.assertIn("::warning::", result.stdout)
                    self.assertEqual(output.read_text(), f"exit_code={rc}\n")

    def test_component_publishes_partial_or_error_evidence_before_failing(self):
        workflow = workflow_text("bench-component.yml")

        def step(name):
            return workflow.split(f"      - name: {name}\n", 1)[1].split(
                "      - name:", 1
            )[0]

        def script(name):
            block = step(name)
            if "        run: |\n" in block:
                return textwrap.dedent(block.split("        run: |\n", 1)[1])
            return block.split("        run: ", 1)[1].splitlines()[0]

        compile_name = "Compile-check this component's bench targets"
        criterion_name = "Run Criterion benches (quick profile, bounded to 10 minutes)"
        record_name = "Record component trend ledger entry"
        publish_name = "Publish ledger to perf-data branch"
        artifact_name = "Upload raw Criterion output"
        final_name = "Report benchmark failures after publication"
        names = [compile_name, criterion_name, record_name, publish_name, artifact_name, final_name]
        positions = [workflow.index(f"      - name: {name}\n") for name in names]
        self.assertEqual(positions, sorted(positions), "failure must follow both publication steps")
        self.assertIn("id: criterion", step(criterion_name))
        for name in (record_name, final_name):
            self.assertIn("if: ${{ !cancelled() }}", step(name))
            self.assertIn("COMPILE_EXIT_CODE: ${{ steps.compile.outputs.exit_code }}", step(name))
            self.assertIn("CRITERION_EXIT_CODE: ${{ steps.criterion.outputs.exit_code }}", step(name))
        self.assertIn("if: always()", step(artifact_name))
        for evidence_path in (
            "component-phases.log", "crates/target/criterion", "bench-data/components.jsonl",
        ):
            self.assertIn(evidence_path, step(artifact_name))
        self.assertIn("github.event_name == 'push'", step(publish_name))

        # Execute the workflow's real shell and real recorder. Only the native
        # benchmark, remote publisher and upload service are fixture boundaries.
        for compile_rc, criterion_rc, estimates in [
            (0, 0, True), (0, 7, True), (0, 124, True), (0, 137, True),
            (0, 7, False), (124, None, False),
            (0, None, True), (0, None, False),
        ]:
            with self.subTest(compile=compile_rc, criterion=criterion_rc, estimates=estimates):
                with tempfile.TemporaryDirectory() as tmp:
                    root = pathlib.Path(tmp)
                    work = root / "crates"
                    work.mkdir()
                    bins = root / "bin"
                    bins.mkdir()
                    timeout = bins / "timeout"
                    timeout.write_text('#!/bin/sh\nexit "$FAKE_RC"\n')
                    timeout.chmod(0o755)
                    publisher = root / "scripts/perf/publish_ledger.sh"
                    publisher.parent.mkdir(parents=True)
                    publisher.write_text(
                        'set -eu\nmkdir published\ncp "$1" published/components.jsonl\n'
                        'echo publish >> "$ORDER_LOG"\n'
                    )
                    env = {
                        **os.environ,
                        "PATH": f"{bins}:{os.environ['PATH']}",
                        "COMPONENT_CRATES": "khive-pack-knowledge",
                        "COMPONENT_JOB_MINUTES": "40",
                        "GITHUB_OUTPUT": str(root / "output"),
                        "GITHUB_STEP_SUMMARY": str(root / "summary"),
                        "GITHUB_SHA": "a" * 40,
                        "GITHUB_REF_NAME": "main",
                        "GITHUB_RUN_ID": "fixture-run",
                        "GITHUB_RUN_ATTEMPT": "1",
                        "ORDER_LOG": str(root / "order"),
                        "FAKE_RC": str(compile_rc),
                    }

                    def run(name, cwd=root):
                        command = script(name).replace(
                            "python3 scripts/perf/bench_track.py",
                            shlex.join([sys.executable, str(REPO_ROOT / "scripts/perf/bench_track.py")]),
                        ).replace("/tmp/components-trend.md", str(root / "trend.md"))
                        return subprocess.run(
                            ["bash", "-c", command], cwd=cwd, env=env,
                            capture_output=True, text=True, timeout=10, check=False,
                        )

                    result = run(compile_name, work)
                    self.assertEqual(result.returncode, compile_rc, result.stderr)
                    env["COMPILE_EXIT_CODE"] = (root / "output").read_text().strip().split("=")[1]
                    env["CRITERION_EXIT_CODE"] = ""
                    if compile_rc == 0:
                        if estimates:
                            estimate = work / "target/criterion/fixture/before_failure/new/estimates.json"
                            estimate.parent.mkdir(parents=True)
                            estimate.write_text(json.dumps({"mean": {"point_estimate": 42.0}}))
                        if criterion_rc is not None:
                            env["FAKE_RC"] = str(criterion_rc)
                            (root / "output").unlink()
                            result = run(criterion_name, work)
                            self.assertEqual(result.returncode, 0, result.stderr)
                            env["CRITERION_EXIT_CODE"] = (root / "output").read_text().strip().split("=")[1]

                    result = run(record_name)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    result = run(publish_name)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    # upload-artifact is an external action: retain its configured
                    # local evidence here before executing the final workflow step.
                    artifact = root / "artifact"
                    artifact.mkdir()
                    shutil.copy(root / "component-phases.log", artifact)
                    shutil.copy(root / "bench-data/components.jsonl", artifact)
                    if estimates:
                        shutil.copytree(work / "target/criterion", artifact / "criterion")
                    with (root / "order").open("a") as order:
                        order.write("artifact\n")

                    result = run(final_name)
                    expected_rc = compile_rc or (criterion_rc if criterion_rc is not None else 1)
                    self.assertEqual(result.returncode, expected_rc, result.stderr)
                    self.assertEqual((root / "order").read_text(), "publish\nartifact\n")
                    self.assertEqual(
                        (root / "published/components.jsonl").read_bytes(),
                        (artifact / "components.jsonl").read_bytes(),
                    )
                    record = json.loads((artifact / "components.jsonl").read_text())
                    self.assertEqual(record["gate_exit_code"], expected_rc)
                    self.assertEqual(record["gate_status"], "fail" if expected_rc else "pass")
                    self.assertEqual(record["status"], "ok" if estimates else "error")
                    self.assertEqual(bool(record["metrics"]), estimates)

    def test_component_compile_failure_still_reaches_diagnostics(self):
        workflow = workflow_text("bench-component.yml")
        compile_step = workflow.split("      - name: Compile-check", 1)[1].split(
            "      - name:", 1
        )[0]
        self.assertIn("id: compile", compile_step)
        self.assertIn("continue-on-error: true", compile_step)
        self.assertIn("if: steps.compile.outcome == 'success'", workflow)
        self.assertIn("cat component-phases.log", workflow)
        self.assertIn("            component-phases.log", workflow)
        self.assertIn("default: 30", workflow)

    def test_component_runner_limits_quick_flag_to_bench_targets(self):
        workflow = workflow_text("bench-component.yml")
        bench_commands = [
            line.strip() for line in workflow.splitlines() if "cargo bench" in line
        ]
        quick_commands = [line for line in bench_commands if "--quick" in line]
        self.assertEqual(len(quick_commands), 1)
        self.assertIn("--benches", quick_commands[0])
        self.assertIn("--criterion-dir crates/target/criterion", workflow)


class FilteredCargoTestWorkflowTests(unittest.TestCase):
    ASSERT_SCRIPT = REPO_ROOT / "scripts" / "assert-tests-ran.sh"
    SUMMARY = "test result: ok. 2 passed; 0 failed; 3 ignored; 0 measured; 9 filtered out; finished in 0.01s\n"
    ZERO_SUMMARY = "running 0 tests\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 10 filtered out; finished in 0.00s\n"

    @staticmethod
    def name_filter(command: str) -> str | None:
        if "cargo" not in command:
            return None
        words = shlex.split(command, comments=True)
        if "cargo" not in words:
            return None
        words = words[words.index("cargo") + 1 :]
        if words and words[0].startswith("+"):
            words = words[1:]
        if not words or words.pop(0) != "test":
            return None
        value_options = {
            "-p",
            "--package",
            "--exclude",
            "--features",
            "-F",
            "--manifest-path",
            "--target",
            "--target-dir",
            "--profile",
            "--bin",
            "--example",
            "--test",
            "--bench",
            "--config",
            "--message-format",
            "--color",
            "-j",
            "--jobs",
            "-Z",
        }
        while words:
            word = words.pop(0)
            if word in {"--", "|", "||", "&&", ";", "2>&1", ">", ">>"}:
                break
            if word in value_options:
                if not words:
                    raise AssertionError(f"missing value for {word}: {command}")
                words.pop(0)
            elif not word.startswith("-"):
                return word
        return None

    @classmethod
    def filtered_steps(cls, workflow: str):
        # Match the repository's indented run scalars, then let shlex distinguish
        # Cargo option values from test-name filters. No YAML dependency is needed
        # by the stdlib-only lint entry point. Unsupported Cargo-bearing scalars
        # fail loudly instead of disappearing from the discovered population.
        lines = workflow.splitlines()
        for index, line in enumerate(lines):
            match = re.fullmatch(r"( *)(- )?run: (.*)", line)
            if not match:
                continue
            indent = len(match[1]) + len(match[2] or "")
            end = index + 1
            while end < len(lines):
                following = lines[end]
                if (
                    following.strip()
                    and len(following) - len(following.lstrip()) <= indent
                ):
                    break
                end += 1
            scalar = match[3]
            continuation = "\n".join(lines[index + 1 : end])
            marker = scalar.split("#", 1)[0].strip()
            if marker in {"|", "|-", "|+"}:
                body = continuation
            elif scalar.startswith(("'", '"', ">", "|")) or "cargo" in continuation:
                if "cargo" in scalar or "cargo" in continuation:
                    raise AssertionError(
                        f"unsupported Cargo run scalar at line {index + 1}"
                    )
                continue
            else:
                body = scalar
            commands = [
                value.strip()
                for value in body.replace("\\\n", " ").splitlines()
                if value.strip() and not value.lstrip().startswith("#")
            ]
            for command_index, command in enumerate(commands):
                name = cls.name_filter(command)
                if name is None:
                    continue
                start = index
                step_indent = indent - 2
                while start > 0 and not lines[start].startswith(
                    " " * step_indent + "- "
                ):
                    start -= 1
                step_end = end
                while step_end < len(lines):
                    following = lines[step_end]
                    if (
                        following.strip()
                        and len(following) - len(following.lstrip()) <= step_indent
                    ):
                        break
                    step_end += 1
                yield (
                    index + 1,
                    name,
                    commands[command_index:],
                    "\n".join(lines[start:step_end]),
                )

    def assert_guarded(self, site):
        line, name, commands, step = site
        label = f"line {line}, filter {name}"
        words = shlex.split(commands[0], comments=True)
        self.assertEqual(words[-4:-1], ["2>&1", "|", "tee"], label)
        log = words[-1]
        self.assertTrue(log.startswith("$RUNNER_TEMP/"), label)
        self.assertGreaterEqual(len(commands), 2, label)
        self.assertEqual(
            shlex.split(commands[1], comments=True),
            ["../scripts/assert-tests-ran.sh", log, "1"],
            label,
        )
        self.assertRegex(step, r"(?m)^\s+shell: bash$", label)
        self.assertNotRegex(step, r"continue-on-error:\s*(?:true|\$)", label)

    def test_every_filtered_cargo_step_has_a_captured_floor(self):
        sites = [
            (path.name, site)
            for path in sorted(WORKFLOWS.glob("*.y*ml"))
            for site in self.filtered_steps(path.read_text())
        ]
        self.assertTrue(sites, "no filtered Cargo tests discovered")
        print(
            f"FilteredCargoTestWorkflowTests: {len(sites)} filtered steps discovered",
            flush=True,
        )
        for path, site in sites:
            with self.subTest(workflow=path, filter=site[1]):
                self.assert_guarded(site)

    def test_discovery_detects_a_new_unguarded_site(self):
        workflow = (
            "jobs:\n  new-job:\n    steps:\n      - name: New filtered suite\n"
            "        run: cargo test -p example --features sample new_filter\n"
        )
        sites = list(self.filtered_steps(workflow))
        self.assertEqual(len(sites), 1)
        self.assertEqual(sites[0][1], "new_filter")
        with self.assertRaises(AssertionError):
            self.assert_guarded(sites[0])

    def test_discovery_distinguishes_filters_from_option_values(self):
        for command in (
            "cargo test --workspace --all-features",
            "cargo test -p sample --test integration -- --ignored",
            "cargo test --target wasm32-wasip1 2>&1 | tee /tmp/result.log",
            "cargo test --features 'one two' --bin example",
        ):
            with self.subTest(command=command):
                self.assertIsNone(self.name_filter(command))
        self.assertEqual(
            self.name_filter(
                "cargo +1.95.0 test --package=sample --lib 'module::test'"
            ),
            "module::test",
        )

    def test_discovery_cannot_skip_cargo_in_block_scalars(self):
        prefix = "jobs:\n  example:\n    steps:\n      - name: Filtered suite\n"
        for marker in ("|", "|-", "| # a comment"):
            with self.subTest(marker=marker):
                workflow = (
                    prefix + f"        run: {marker}\n          cargo test new_filter\n"
                )
                sites = list(self.filtered_steps(workflow))
                self.assertEqual(len(sites), 1)
                self.assertEqual(sites[0][1], "new_filter")
        for marker in (">", "|2", "&anchored |"):
            with self.subTest(marker=marker):
                workflow = (
                    prefix + f"        run: {marker}\n          cargo test new_filter\n"
                )
                with self.assertRaisesRegex(
                    AssertionError, "unsupported Cargo run scalar"
                ):
                    list(self.filtered_steps(workflow))

    def test_removing_each_discovered_assertion_is_detected(self):
        for path in sorted(WORKFLOWS.glob("*.y*ml")):
            for site in self.filtered_steps(path.read_text()):
                line, name, commands, step = site
                with self.subTest(workflow=path.name, filter=name):
                    self.assert_guarded(site)
                    with self.assertRaises(AssertionError):
                        self.assert_guarded((line, name, commands[:1], step))

    def run_assertion(self, content: str | None, floor="1", *, fake_awk=None):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            log = root / "selected_filter.log"
            if content is not None:
                log.write_text(content)
            env = os.environ.copy()
            if fake_awk is not None:
                awk = root / "awk"
                awk.write_text("#!/bin/sh\n" + fake_awk)
                awk.chmod(0o755)
                env["PATH"] = f"{root}:{env['PATH']}"
            return subprocess.run(
                ["bash", str(self.ASSERT_SCRIPT), str(log), floor],
                capture_output=True,
                text=True,
                check=False,
                env=env,
            )

    def test_nonzero_summary_passes_and_reports_count(self):
        for count in (2, 82, 717):
            with self.subTest(count=count):
                result = self.run_assertion(
                    self.SUMMARY.replace("2 passed", f"{count} passed")
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn(f"counted={count} floor=1", result.stdout)
                self.assertIn("selected_filter", result.stdout)

    def test_sums_passed_and_failed_across_harnesses(self):
        result = self.run_assertion(
            self.SUMMARY
            + "test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.20s\n",
            "3",
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("counted=3 floor=3", result.stdout)
        self.assertNotEqual(self.run_assertion(self.SUMMARY, "3").returncode, 0)

    def test_colored_crlf_summaries_are_counted_and_validated(self):
        summary = self.SUMMARY.replace("ok.", "\x1b[32mok\x1b[0m.").replace(
            "\n", "\r\n"
        )
        result = self.run_assertion(summary)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("counted=2 floor=1", result.stdout)
        malformed = "\x1b[32mtest result: ok. 717junk passed; 0 failed;\x1b[0m\r\n"
        self.assertNotEqual(self.run_assertion(summary + malformed).returncode, 0)

    def test_truncated_and_trailing_text_summaries_are_malformed(self):
        # A summary cut off after the failed count, or followed by anything after
        # the cargo grammar, is not a summary; it must fail closed, not count.
        for content in (
            "test result: ok. 1 passed; 0 failed;\n",
            "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured;\n",
            "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s extra\n",
            "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s; more\n",
        ):
            with self.subTest(content=content):
                result = self.run_assertion(self.SUMMARY + content)
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertIn("malformed test summary", result.stderr)
        without_timing = self.SUMMARY.replace("; finished in 0.01s", "")
        self.assertEqual(self.run_assertion(without_timing).returncode, 0)

    def test_zero_selection_and_no_summary_fail(self):
        for content in (self.ZERO_SUMMARY, "", "running 1 test\n"):
            with self.subTest(content=content):
                result = self.run_assertion(content)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("counted=0 floor=1", result.stdout + result.stderr)

    def test_result_module_test_names_are_not_summaries(self):
        test_line = "test result::tests::default_is_zero_state_zero_count ... ok\n"
        result = self.run_assertion(test_line + self.SUMMARY)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("counted=2 floor=1", result.stdout)
        for malformed in (
            "test result:ok. 1 passed; 0 failed;\n",
            "test result:garbage\n",
        ):
            with self.subTest(malformed=malformed):
                result = self.run_assertion(self.SUMMARY + malformed)
                self.assertEqual(result.returncode, 2, result.stdout)
                self.assertIn("malformed test summary", result.stderr)

    def test_any_malformed_summary_fails_even_beside_a_valid_one(self):
        for passed, failed in (
            ("717junk", "0"),
            ("unreadable", "unknown"),
            ("1", "0junk"),
            ("-1", "0"),
            ("1.5", "0"),
            ("", "0"),
            ("1", ""),
        ):
            with self.subTest(passed=passed, failed=failed):
                malformed = f"test result: ok. {passed} passed; {failed} failed; 0 ignored; 0 measured; 0 filtered out\n"
                result = self.run_assertion(self.SUMMARY + malformed)
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertIn("floor=1", result.stdout + result.stderr)

    def test_missing_log_and_invalid_floor_fail(self):
        self.assertNotEqual(self.run_assertion(None).returncode, 0)
        for floor in ("0", "-1", "", "1junk", "1.5", "99999999999999999999"):
            with self.subTest(floor=floor):
                self.assertNotEqual(
                    self.run_assertion(self.SUMMARY, floor).returncode, 0
                )

    def test_parser_failure_cannot_pass_with_plausible_stdout(self):
        result = self.run_assertion(self.SUMMARY, fake_awk="printf '100\\n'\nexit 7\n")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("parser exited 7", result.stderr)

    def test_invalid_parser_output_and_count_overflow_fail(self):
        for output in ("junk", "08", "99999999999999999999", "1\\n2"):
            with self.subTest(output=output):
                result = self.run_assertion(
                    self.SUMMARY, fake_awk=f"printf '{output}\\n'\n"
                )
                self.assertNotEqual(result.returncode, 0, result.stdout)
        result = self.run_assertion(
            self.SUMMARY
            + "test result: ok. 2147483647 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n"
        )
        self.assertNotEqual(result.returncode, 0, result.stdout)

    def test_unreadable_log_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            log = pathlib.Path(directory) / "unreadable.log"
            log.write_text(self.SUMMARY)
            log.chmod(0)
            try:
                if os.access(log, os.R_OK):
                    self.skipTest("this account can read mode-000 files")
                result = subprocess.run(
                    ["bash", str(self.ASSERT_SCRIPT), str(log), "1"],
                    capture_output=True,
                    text=True,
                    check=False,
                )
            finally:
                log.chmod(0o600)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unreadable", result.stderr)

    def test_helper_keeps_lf_with_windows_checkout_conversion(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            repo = root / "repo"
            checkout = root / "checkout"
            repo.mkdir()
            checkout.mkdir()
            script = pathlib.Path("scripts/assert-tests-ran.sh")
            source = repo / script
            source.parent.mkdir()
            source.write_bytes(self.ASSERT_SCRIPT.read_bytes())
            source.chmod(self.ASSERT_SCRIPT.stat().st_mode & 0o777)
            attributes = REPO_ROOT / ".gitattributes"
            staged = [str(script)]
            if attributes.is_file():
                (repo / attributes.name).write_bytes(attributes.read_bytes())
                staged.append(attributes.name)
            env = os.environ.copy()
            env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull)
            for args in (
                ["init", "--quiet"],
                ["config", "core.autocrlf", "true"],
                ["add", "--", *staged],
                ["checkout-index", f"--prefix={checkout}{os.sep}", "--all"],
            ):
                result = subprocess.run(
                    ["git", *args],
                    cwd=repo,
                    env=env,
                    capture_output=True,
                    text=True,
                    check=False,
                )
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            exported = checkout / script
            self.assertEqual(
                exported.read_bytes().count(b"\r\n"),
                0,
                f"{script} was converted to CRLF under core.autocrlf=true",
            )
            log = root / "selected_filter.log"
            log.write_text(self.SUMMARY)
            result = subprocess.run(
                [str(exported), str(log), "1"],
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("counted=2 floor=1", result.stdout)

    def test_each_discovered_step_runs_the_gate_and_preserves_pipeline_failures(self):
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            cargo = root / "cargo"
            cargo.write_text(
                '#!/bin/sh\nprintf "%s\\n" "$TEST_SUMMARY"\nexit "$TEST_STATUS"\n'
            )
            cargo.chmod(0o755)
            env = os.environ.copy()
            env.update(PATH=f"{root}:{env['PATH']}", RUNNER_TEMP=str(root))
            sites = [
                site
                for path in sorted(WORKFLOWS.glob("*.y*ml"))
                for site in self.filtered_steps(path.read_text())
            ]
            self.assertTrue(sites)
            for _, name, commands, _ in sites:
                for summary, cargo_status, expected in (
                    (self.SUMMARY, "0", 0),
                    (self.ZERO_SUMMARY, "0", 1),
                    (self.SUMMARY, "7", 7),
                ):
                    with self.subTest(
                        filter=name, summary=summary, status=cargo_status
                    ):
                        env.update(TEST_SUMMARY=summary, TEST_STATUS=cargo_status)
                        result = subprocess.run(
                            [
                                "bash",
                                "--noprofile",
                                "--norc",
                                "-eo",
                                "pipefail",
                                "-c",
                                "\n".join(commands),
                            ],
                            cwd=REPO_ROOT / "crates",
                            env=env,
                            capture_output=True,
                            text=True,
                            check=False,
                        )
                        self.assertEqual(
                            result.returncode, expected, result.stdout + result.stderr
                        )
            tee = root / "tee"
            tee.write_text("#!/bin/sh\ncat >/dev/null\nexit 9\n")
            tee.chmod(0o755)
            env.update(TEST_SUMMARY=self.SUMMARY, TEST_STATUS="0")
            for _, name, commands, _ in sites:
                with self.subTest(filter=name, failure="tee"):
                    result = subprocess.run(
                        [
                            "bash",
                            "--noprofile",
                            "--norc",
                            "-eo",
                            "pipefail",
                            "-c",
                            "\n".join(commands),
                        ],
                        cwd=REPO_ROOT / "crates",
                        env=env,
                        capture_output=True,
                        text=True,
                        check=False,
                    )
                    self.assertEqual(
                        result.returncode, 9, result.stdout + result.stderr
                    )

    def test_bare_success_check_cannot_distinguish_zero_selection(self):
        with tempfile.TemporaryDirectory() as directory:
            log = pathlib.Path(directory) / "selected_filter.log"
            log.write_text(self.ZERO_SUMMARY)
            bare = subprocess.run(
                ["bash", "-c", 'cat "$1"; exit 0', "cargo-style", str(log)],
                capture_output=True,
                text=True,
                check=False,
            )
            guarded = subprocess.run(
                ["bash", str(self.ASSERT_SCRIPT), str(log), "1"],
                capture_output=True,
                text=True,
                check=False,
            )
        self.assertEqual(bare.returncode, 0)
        self.assertNotEqual(guarded.returncode, 0)


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
        # The smoke default pack set is read from `RuntimeConfig::built_in_packs`,
        # so a list typed here would be a third copy of the declaration and would
        # go stale the day a pack lands. Re-derive it independently instead: parse
        # the declaration with a different expression than the helper uses, and
        # require a core floor so a parse that silently yields nothing fails here.
        declaration = (
            REPO_ROOT / "crates" / "khive-runtime" / "src" / "config.rs"
        ).read_text()
        body = declaration.split("pub fn built_in_packs() -> Vec<String> {", 1)[1]
        body = body.split("]", 1)[0]
        declared = {
            token
            for token in body.split('"')[1::2]
            if token and all(c.isalpha() or c == "_" for c in token)
        }
        self.assertLessEqual({"kg", "gtd", "comm", "memory", "workspace"}, declared)
        self.assertEqual(set(self.smoke.DEFAULT_PACKS), declared)

    def test_no_new_hand_typed_copy_of_the_shipping_pack_set(self):
        """The shipping pack set had four copies. One was replaced by a read of
        the declaration, one reddened this suite two packs behind, one silently
        shrank a gate fixture's registry, and one is a revision-pinned census
        manifest. A fifth copy would be found by the next outage, so this fails
        the moment a file outside the allow-list starts naming the set.

        Allow-list entries are not exemptions from being correct: each one
        states why it is allowed to carry the names, and three of them assert
        against the declaration in their own suite.
        """
        allowed = {
            # The declaration itself.
            "crates/khive-runtime/src/config.rs",
            # Gate fixture; its own unit test asserts equality with the declaration.
            "crates/kkernel/src/code_ingest.rs",
            # Revision-pinned census manifests, regenerated by scripts/writer_census.py.
            "scripts/data/writer-census-v1.json",
            "crates/khive-runtime/tests/data/adr133-writer-census.json",
            # An accepted ADR is a historical record; it is amended, never rewritten.
            "docs/adr/ADR-027-dynamic-pack-loading.md",
            # Reference pages that enumerate the shipped packs for a reader.
            "docs/guide/api-reference.md",
            "crates/khive-pack-kg/README.md",
            # This file: the allow-list above names the packs by construction.
            "scripts/tests/test_ci_workflows.py",
            # An enumerated contract test whose FIRST assertion is equality with
            # the declaration, so the enumeration below it cannot drift silently.
            "crates/khive-runtime/src/runtime.rs",
            # Same shape: the description scan pairs each pack name with the
            # handler table it publishes, and its first assertion is that the
            # names cover built_in_packs() with a declared, checked remainder.
            "crates/kkernel/tests/descriptions_are_written_for_callers.rs",
        }
        names = {
            "kg", "gtd", "memory", "brain", "comm", "schedule", "knowledge",
            "session", "git", "code", "workspace", "blob", "tool", "exec",
        }
        token = re.compile(r'["`]([a-z_]+)["`]')
        offenders = []
        for base in ("crates", "scripts", "tests", "python", "docs"):
            root = REPO_ROOT / base
            if not root.is_dir():
                continue
            for path in root.rglob("*"):
                if path.is_dir() or "target" in path.parts or "__pycache__" in path.parts:
                    continue
                if path.suffix not in {".rs", ".py", ".md", ".json", ".toml"}:
                    continue
                rel = path.relative_to(REPO_ROOT).as_posix()
                if rel in allowed:
                    continue
                try:
                    lines = path.read_text(errors="replace").splitlines()
                except OSError:
                    continue
                for i in range(len(lines)):
                    window = "\n".join(lines[i : i + 20])
                    if len(set(token.findall(window)) & names) >= 9:
                        offenders.append(f"{rel}:{i + 1}")
                        break
        self.assertEqual(
            [],
            offenders,
            "a new hand-typed copy of the shipping pack set appeared; read it from "
            "RuntimeConfig::built_in_packs() instead, or add the path to the allow-list "
            "above with the reason it is allowed to carry the names",
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


class DocsFormatPhaseTests(unittest.TestCase):
    def run_docs_fmt(self, path_value):
        env = os.environ.copy()
        env["PATH"] = path_value
        return subprocess.run(
            ["sh", str(REPO_ROOT / "scripts" / "ci.sh"), "docs-fmt"],
            cwd=REPO_ROOT,
            env=env,
            check=False,
            capture_output=True,
            text=True,
        )

    def test_docs_job_runs_the_committed_phase(self):
        # The hosted docs job and a local run have to execute one command, not
        # two copies of it: the workflow carried its own `deno fmt --check` for
        # long enough that a hand-padded markdown table passed every phase in
        # ci.sh and failed on the hosted side.
        docs_job = indented_block(workflow_text("ci.yml"), "docs", 2)
        self.assertIn("run: scripts/ci.sh docs-fmt", docs_job)
        self.assertNotIn("deno fmt", docs_job)
        self.assertIn("denoland/setup-deno@v2", docs_job)

    def test_docs_fmt_refuses_when_deno_is_absent(self):
        # The absence arm decides whether this gate can be emptied in silence: a
        # skip would exit 0 and check nothing. The stub arm runs the identical
        # invocation with a deno on PATH, so the pair shows the refusal comes
        # from the missing binary rather than from the phase never running, and
        # it reads back the argv to prove the phase checks rather than formats.
        minimal_path = "/usr/bin:/bin"
        for directory in minimal_path.split(":"):
            if pathlib.Path(directory, "deno").exists():
                self.skipTest(f"deno is installed in {directory}, which the absence arm empties")

        with tempfile.TemporaryDirectory() as tmp:
            stub_dir = pathlib.Path(tmp, "bin")
            stub_dir.mkdir()
            argv_log = pathlib.Path(tmp, "argv")
            stub = stub_dir / "deno"
            stub.write_text(
                "#!/bin/sh\n"
                f'printf "%s\\n" "$@" > {shlex.quote(str(argv_log))}\n'
                "exit 0\n"
            )
            stub.chmod(0o755)

            absent = self.run_docs_fmt(minimal_path)
            self.assertEqual(absent.returncode, 1, absent.stdout + absent.stderr)
            self.assertIn("deno not found on PATH", absent.stderr)
            self.assertFalse(argv_log.exists())

            present = self.run_docs_fmt(f"{stub_dir}:{minimal_path}")
            self.assertEqual(present.returncode, 0, present.stdout + present.stderr)
            self.assertEqual(argv_log.read_text().split(), ["fmt", "--check"])


class NpmReleaseWorkflowTests(unittest.TestCase):
    def test_release_publishes_cli_alias_after_exact_version_umbrella(self):
        workflow = workflow_text("release.yml")

        self.assertIn("ALIAS_VERSION=$(node -p", workflow)
        self.assertIn("ALIAS_KHIVE_VERSION=$(node -p", workflow)
        self.assertIn('if [ "$VERSION" != "$ALIAS_VERSION" ]', workflow)
        self.assertIn('if [ "$VERSION" != "$ALIAS_KHIVE_VERSION" ]', workflow)

        umbrella_publish = workflow.index("- name: Publish khive (umbrella)")
        alias_rewrite = workflow.index(
            "- name: Set CLI alias version and khive dependency"
        )
        alias_publish = workflow.index(
            "- name: Publish @khive-ai/cli (compatibility alias)"
        )
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
                'echo "unexpected npm command: $*" >&2\n'
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
        version = json.loads((REPO_ROOT / "npm" / "package.json").read_text())[
            "version"
        ]
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
                'if [ "${1:-}" = view ]; then\n'
                '  case "${2:-}" in @khive-ai/cli@*) echo 0.0.1; exit 0;; esac\n'
                "  exit 1\n"
                "fi\n"
                'echo "unexpected npm command: $*" >&2\n'
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

        version = json.loads((REPO_ROOT / "npm" / "package.json").read_text())[
            "version"
        ]
        self.assertEqual(completed.returncode, 1, completed.stdout)
        self.assertIn(f"depends on khive 0.0.1, expected {version}", completed.stderr)
        self.assertNotIn("would publish @khive-ai/cli", completed.stdout)

    def test_local_publish_stops_when_the_alias_lookup_fails_for_another_reason(self):
        publish_script = REPO_ROOT / "scripts" / "npm-publish.sh"
        with tempfile.TemporaryDirectory() as temp_dir:
            npm_stub = pathlib.Path(temp_dir) / "npm"
            npm_stub.write_text(
                "#!/bin/sh\n"
                'if [ "${1:-}" = view ]; then\n'
                "  case \"${2:-}\" in @khive-ai/cli@*) echo 'npm ERR! code ECONNREFUSED' >&2; exit 1;; esac\n"
                "  echo 'npm ERR! code E404' >&2; exit 1\n"
                "fi\n"
                'echo "unexpected npm command: $*" >&2\n'
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


class HistoricalReplayWorkflowTests(unittest.TestCase):
    CHECKOUT_NAMES = {
        "ci": "CI (${{ matrix.os }}, shard ${{ matrix.shard }}/2)",
        "khive-py": "Python client tests",
        "kg-editor": "KG Studio (Next.js)",
        "supply-chain": "Supply-chain (cargo-deny)",
        "data-leak-guard": "JSON/JSONL data-leak guard",
        "secret-scan": "Secret scan (gitleaks)",
        "docs": "Docs lint",
        "marketplace": "Marketplace example validator",
        "doc-build": "Doc build (-D warnings)",
        "dependency-review": "Dependency review",
        "wasm-parity": "wasm-parity (khive-changeset)",
        "vamana-portability": "vamana portability (ADR-110 Layer A)",
        "minio-blob-compat": "MinIO BlobStore compatibility (ADR-111 Amendment 2)",
        "check-windows": "Windows compile check",
        "msrv-check": "MSRV compile check",
        "coverage-measurement": "Coverage measurement",
        "coverage-ratchet": "Coverage ratchet",
    }

    def setUp(self):
        self.workflow = workflow_text("ci.yml")
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = pathlib.Path(self.temp.name)
        self.output = self.root / "output"
        self.summary = self.root / "summary"
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith("GIT_")}
        self.env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull,
                        GITHUB_OUTPUT=str(self.output), GITHUB_STEP_SUMMARY=str(self.summary))

    def script(self, job_name, step_name):
        job = indented_block(self.workflow, job_name, 2)
        step = job.split(f"- name: {step_name}\n", 1)[1]
        match = re.search(r"(?m)^        run: \|\n((?:          .*\n|\n)+)", step + "\n")
        self.assertIsNotNone(match, f"missing executable step {step_name}")
        return textwrap.dedent(match.group(1))

    def run_script(self, script, *, cwd=None, **env):
        self.output.write_text("")
        self.summary.write_text("")
        completed = subprocess.run(
            ["bash", "-e", "-c", script], cwd=cwd or self.root,
            env={**self.env, **env}, text=True, capture_output=True,
            timeout=10, check=False,
        )
        return completed, self.output.read_text()

    def git(self, cwd, *args):
        return subprocess.run(
            ["git", "-c", "core.hooksPath=" + os.devnull, *args], cwd=cwd,
            env=self.env, text=True, capture_output=True, timeout=10, check=True,
        ).stdout.strip()

    def repository(self):
        remote = self.root / "origin.git"
        work = self.root / "work"
        self.git(self.root, "init", "--bare", "--initial-branch=main", str(remote))
        self.git(self.root, "init", "--initial-branch=main", str(work))
        self.git(work, "config", "user.name", "CI replay fixture")
        self.git(work, "config", "user.email", "ci-replay@example.invalid")
        self.git(work, "remote", "add", "origin", str(remote))
        (work / "fixture").write_text("parent\n")
        self.git(work, "add", "fixture")
        self.git(work, "commit", "-m", "parent")
        parent = self.git(work, "rev-parse", "HEAD")
        (work / "fixture").write_text("tip\n")
        self.git(work, "commit", "-am", "tip")
        tip = self.git(work, "rev-parse", "HEAD")
        self.git(work, "tag", "-a", "fixture-tag", "-m", "annotated tag")
        tag = self.git(work, "rev-parse", "fixture-tag")
        tree = self.git(work, "rev-parse", "HEAD^{tree}")
        blob = self.git(work, "rev-parse", "HEAD:fixture")
        self.git(work, "checkout", "-b", "side", parent)
        (work / "fixture").write_text("side\n")
        self.git(work, "commit", "-am", "side")
        side = self.git(work, "rev-parse", "HEAD")
        self.git(work, "checkout", "main")
        self.git(work, "push", "origin", "main", "side", "--tags")
        return work, {"parent": parent, "tip": tip, "side": side,
                      "tag": tag, "tree": tree, "blob": blob}

    def test_input_is_strict_hex_and_never_shell_source(self):
        script = self.script("resolve-revision", "Validate replay input")
        self.assertNotIn("${{", script, "input must be passed through environment")
        for value in ["a" * 40, "F" * 40]:
            result, output = self.run_script(script, EVENT_NAME="workflow_dispatch", REPLAY_SHA=value)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(output, "replay=true\n")
        marker = self.root / "injected"
        for value in ["main", "a" * 39, "a" * 41, "g" * 40, "--help",
                      "a" * 40 + "\n", " " + "a" * 40, "a" * 40 + "^",
                      f"$(touch {marker})", f"`touch {marker}`", f"'; touch {marker}; #"]:
            with self.subTest(value=value):
                result, output = self.run_script(script, EVENT_NAME="workflow_dispatch", REPLAY_SHA=value)
                self.assertNotEqual(result.returncode, 0, "invalid replay input must be rejected")
                self.assertIn("exactly 40 hexadecimal", result.stderr)
                self.assertEqual(output, "")
                self.assertFalse(marker.exists(), "input executed a shell command")

    def test_ordinary_events_and_empty_dispatch_keep_event_checkout(self):
        script = self.script("resolve-revision", "Validate replay input")
        for event, value in [("workflow_dispatch", ""), ("push", ""),
                             ("pull_request", "untrusted"), ("schedule", "")]:
            result, output = self.run_script(script, EVENT_NAME=event, REPLAY_SHA=value)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(output, "replay=false\n")
        resolver = indented_block(self.workflow, "resolve-revision", 2)
        self.assertEqual(resolver.count("if: steps.request.outputs.replay == 'true'"), 2)
        self.assertIn("checkout_sha: ${{ steps.resolve.outputs.checkout_sha }}", resolver)
        self.assertNotIn("github.sha", resolver.split("outputs:", 1)[1].split("steps:", 1)[0])

    def test_validator_accepts_main_ancestors_without_changing_checkout(self):
        work, commits = self.repository()
        script = self.script("resolve-revision", "Validate main ancestry")
        self.assertNotIn("${{", script, "candidate must be passed through environment")
        refs_before = self.git(work, "ls-remote", "origin")
        for key in ["parent", "tip"]:
            result, output = self.run_script(script, cwd=work, REPLAY_SHA=commits[key].upper())
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(output, f"checkout_sha={commits[key]}\n")
            self.assertIn(commits[key], self.summary.read_text())
            self.assertEqual(self.git(work, "rev-parse", "HEAD"), commits["tip"])
        self.assertEqual(self.git(work, "ls-remote", "origin"), refs_before)

    def test_validator_rejects_non_main_commit(self):
        work, commits = self.repository()
        result, output = self.run_script(
            self.script("resolve-revision", "Validate main ancestry"),
            cwd=work, REPLAY_SHA=commits["side"],
        )
        self.assertNotEqual(result.returncode, 0, "non-main commit must be rejected")
        self.assertIn("reachable from origin/main", result.stderr)
        self.assertEqual(output, "")

    def test_validator_rejects_non_commit_object_ids(self):
        work, commits = self.repository()
        for value in [commits["tag"], commits["tree"], commits["blob"], "0" * 40]:
            with self.subTest(value=value):
                result, output = self.run_script(
                    self.script("resolve-revision", "Validate main ancestry"),
                    cwd=work, REPLAY_SHA=value,
                )
                self.assertNotEqual(result.returncode, 0, "non-commit object must be rejected")
                self.assertIn("must identify a commit object", result.stderr)
                self.assertEqual(output, "")

    def test_every_ci_checkout_uses_the_validated_sha_after_validation(self):
        checkout_jobs = set()
        for name in re.findall(r"(?m)^  ([a-z][a-z0-9-]*):$", self.workflow.split("jobs:\n", 1)[1]):
            job = indented_block(self.workflow, name, 2)
            if "uses: actions/checkout@v7" not in job:
                continue
            checkout_jobs.add(name)
            if name == "resolve-revision":
                self.assertLess(job.index("- name: Validate replay input"), job.index("uses: actions/checkout@v7"))
                self.assertIn("ref: ${{ github.sha }}\n          fetch-depth: 0", job)
                self.assertNotIn("run: scripts/", job)
                continue
            self.assertRegex(job, r"(?m)^    needs: (?:resolve-revision|\[resolve-revision, coverage-measurement\])$",
                             "every source job must wait for validation")
            self.assertEqual(job.count("uses: actions/checkout@v7"), 1)
            self.assertIn("ref: ${{ needs.resolve-revision.outputs.checkout_sha }}", job,
                          "every CI checkout must use the validated SHA")
        self.assertEqual(checkout_jobs, set(self.CHECKOUT_NAMES) | {"resolve-revision"})
        gate = indented_block(self.workflow, "ci-gate", 2)
        self.assertIn("- resolve-revision", indented_block(gate, "needs", 4),
                      "aggregate must require revision validation")
        self.assertNotIn('"resolve-revision"', gate.split("as $allowed_skips", 1)[0].split("bad=", 1)[1])

    def test_replay_check_names_are_distinct_and_ordinary_names_unchanged(self):
        for name, ordinary in {**self.CHECKOUT_NAMES, "ci-gate": "CI gate"}.items():
            with self.subTest(job=name):
                job = indented_block(self.workflow, name, 2)
                self.assertIn(f"name: {REPLAY_NAME_PREFIX}{ordinary}\n", job,
                              "replay checks must not reuse ordinary required check names")
        self.assertIn("format('CI replay {0}', inputs.main_sha) || ''", self.workflow)
        dispatch = indented_block(indented_block(self.workflow, "on", 0), "workflow_dispatch", 2)
        self.assertIn("main_sha:", dispatch)
        self.assertIn("required: false", dispatch)
        self.assertIn('default: ""', dispatch)

    def test_conditional_check_names_are_admitted_before_work_eligibility(self):
        # Hosted R2 evidence showed literal name expressions when job-level
        # eligibility skipped a job. This prevents that source pattern; only a
        # hosted run can establish how GitHub renders the resulting check names.
        for name in [*self.CHECKOUT_NAMES, "ci-gate"]:
            with self.subTest(job=name):
                job = indented_block(self.workflow, name, 2)
                conditions = re.findall(r"(?m)^    if: (.+)$", job)
                self.assertNotRegex(job, r"(?m)^    if: [>|]",
                                    "CHECK_NAME_ADMISSION: no folded eligibility predicate")
                self.assertIn(conditions, [[], ["always()"]],
                              "CHECK_NAME_ADMISSION: eligibility belongs on steps")
                if name in {"dependency-review", "coverage-ratchet"}:
                    self.assertEqual(
                        conditions, ["always()"],
                        "CHECK_NAME_ADMISSION: admit even after failed or skipped dependencies",
                    )

    def test_dependency_review_work_is_pr_only_after_revision_validation(self):
        job = indented_block(self.workflow, "dependency-review", 2)
        expected = "needs.resolve-revision.result == 'success' && github.event_name == 'pull_request'"
        for name in ["Checkout dependency review revision", "Dependency review"]:
            with self.subTest(step=name):
                step = step_block(job, name)
                self.assertEqual(
                    re.findall(r"(?m)^        if: (.+)$", step), [expected],
                    "DEPENDENCY_REVIEW_ELIGIBILITY: checkout and action remain validated PR-only work",
                )
        self.assertEqual(
            mapping_entries(indented_block(job, "permissions", 4)), {"contents: read"},
            "DEPENDENCY_REVIEW_PERMISSIONS: review stays read-only",
        )
        review = step_block(job, "Dependency review")
        self.assertIn("uses: actions/dependency-review-action@v4", review)
        self.assertIn("fail-on-severity: high", review)
        self.assertIn("allow-licenses:", review)
        self.assertIn("allow-dependencies-licenses: pkg:pypi/typing-extensions, pkg:cargo/sqlite-vec", review)
        self.assertNotIn("continue-on-error", job,
                         "DEPENDENCY_REVIEW_FAILURES: real review failures must fail the job")

    def test_non_pr_dependency_review_explicitly_reports_successful_no_work(self):
        job = indented_block(self.workflow, "dependency-review", 2)
        step = step_block(job, "Report dependency review not applicable")
        self.assertEqual(
            re.findall(r"(?m)^        if: (.+)$", step),
            ["needs.resolve-revision.result == 'success' && github.event_name != 'pull_request'"],
            "DEPENDENCY_REVIEW_NO_WORK: only validated non-PR runs take this path",
        )
        script = self.script("dependency-review", "Report dependency review not applicable")
        self.assertNotIn("${{", script)
        result, _ = self.run_script(script)
        self.assertEqual(result.returncode, 0,
                         "DEPENDENCY_REVIEW_NO_WORK: ineligible work completes successfully")
        self.assertIn("no dependency comparison was run", result.stdout,
                      "DEPENDENCY_REVIEW_NO_WORK: no-work must be explicit")

    def test_failed_revision_is_a_named_dependency_review_failure(self):
        job = indented_block(self.workflow, "dependency-review", 2)
        step = step_block(job, "Refuse unvalidated dependency review revision")
        self.assertEqual(
            re.findall(r"(?m)^        if: (.+)$", step),
            ["needs.resolve-revision.result != 'success'"],
            "UNVALIDATED_REVISION_REJECTED: all non-success validation outcomes fail closed",
        )
        self.assertLess(job.index("- name: Refuse unvalidated dependency review revision"),
                        job.index("uses: actions/checkout@v7"))
        result, _ = self.run_script(self.script("dependency-review", "Refuse unvalidated dependency review revision"))
        self.assertNotEqual(result.returncode, 0,
                            "UNVALIDATED_REVISION_REJECTED: failed selection must not pass a check")
        self.assertIn("dependency review did not run", result.stderr)

    def test_coverage_work_requires_a_validated_available_measurement(self):
        job = indented_block(self.workflow, "coverage-ratchet", 2)
        expected = (
            "needs.resolve-revision.result == 'success' && "
            "needs.coverage-measurement.result == 'success' && "
            "needs.coverage-measurement.outputs.available == 'true'"
        )
        for name in ["Checkout coverage baseline revision", "Check coverage does not regress"]:
            with self.subTest(step=name):
                self.assertEqual(
                    re.findall(r"(?m)^        if: (.+)$", step_block(job, name)), [expected],
                    "COVERAGE_WORK_ELIGIBILITY: validation and measurement must both succeed",
                )
        self.assertNotIn("continue-on-error", job,
                         "COVERAGE_FAILURES: a regression must fail the job")

    def test_unavailable_coverage_is_a_named_failure_without_a_judgment(self):
        job = indented_block(self.workflow, "coverage-ratchet", 2)
        step = step_block(job, "Refuse unavailable coverage input")
        self.assertEqual(
            re.findall(r"(?m)^        if: (.+)$", step),
            ["needs.resolve-revision.result != 'success' || "
             "needs.coverage-measurement.result != 'success' || "
             "needs.coverage-measurement.outputs.available != 'true'"],
            "UNAVAILABLE_COVERAGE_REJECTED: failure, cancellation, skip and missing output remain closed",
        )
        self.assertLess(job.index("- name: Refuse unavailable coverage input"),
                        job.index("uses: actions/checkout@v7"))
        result, _ = self.run_script(self.script("coverage-ratchet", "Refuse unavailable coverage input"))
        self.assertNotEqual(result.returncode, 0,
                            "UNAVAILABLE_COVERAGE_REJECTED: absent inputs cannot produce success")
        self.assertIn("no coverage-regression judgment was made", result.stderr)

    def test_replay_secret_scan_excludes_future_and_other_branch_commits(self):
        work, commits = self.repository()
        script = self.script("secret-scan", "Resolve scan scope")
        script = script.replace("${{ github.event_name }}", "workflow_dispatch")
        script = script.replace("${{ github.event.before }}", "").replace("${{ github.sha }}", commits["tip"])
        result, output = self.run_script(script, cwd=work, REPLAY_SHA=commits["parent"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(output, f"range={commits['parent']}\n",
                         "replay scan must use only the selected commit history")
        scope = self.git(work, "rev-list", output.strip().removeprefix("range="))
        self.assertEqual(scope, commits["parent"])
        result, output = self.run_script(script, cwd=work, REPLAY_SHA="")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(output, "range=\n")
        nightly = self.script("secret-scan", "Resolve scan scope").replace("${{ github.event_name }}", "schedule")
        nightly = nightly.replace("${{ github.event.before }}", "").replace("${{ github.sha }}", commits["tip"])
        result, output = self.run_script(nightly, cwd=work, REPLAY_SHA="")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(output, "range=\n")
        job = indented_block(self.workflow, "secret-scan", 2)
        self.assertIn("REPLAY_SHA: ${{ needs.resolve-revision.outputs.checkout_sha }}", job)
        self.assertIn('range="${base}..${head}"', job)
        self.assertIn('range="${before}..${{ github.sha }}"', job)
        minio = indented_block(self.workflow, "minio-blob-compat", 2)
        self.assertIn("no comparable base SHA -- running the MinIO leg to be safe", minio)


if __name__ == "__main__":
    unittest.main()

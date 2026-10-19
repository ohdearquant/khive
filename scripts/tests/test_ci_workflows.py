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
# `git commit` and `git fetch` start a detached `git maintenance run --auto` that can still be writing
# under `.git` when a fixture's temporary directory is removed, which fails the cleanup. Every fixture
# repository in this file turns automatic maintenance off.
NO_AUTO_MAINTENANCE = {
    "GIT_CONFIG_COUNT": "1",
    "GIT_CONFIG_KEY_0": "maintenance.auto",
    "GIT_CONFIG_VALUE_0": "false",
}


def commit_maintenance_runs(repo: pathlib.Path, env: dict[str, str], name: str) -> list[str]:
    """Commit one new file in `repo` under `env`; return the trace2 events naming a `git maintenance run`."""
    trace = repo.parent / f"{name}.trace2.json"
    (repo / f"{name}.txt").write_text(f"{name}\n")
    for args in (["add", "--", f"{name}.txt"], ["commit", "-q", "-m", name]):
        subprocess.run(
            ["git", "-c", "core.hooksPath=" + os.devnull, *args], cwd=repo,
            env={**env, "GIT_TRACE2_EVENT": str(trace)}, text=True, capture_output=True,
            timeout=10, check=True,
        )
    return [line for line in trace.read_text().splitlines() if '"maintenance","run"' in line]


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
            "maintenance.auto": "false",
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

    def test_fixture_git_turns_off_auto_maintenance(self):
        checkout, _base, env = self._shallow_fixture()
        result = self._git(checkout, env, "config", "--get", "maintenance.auto")
        self.assertEqual(result.stdout.strip(), "false")

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

        self.assertIn("needs: [resolve-revision, coverage-measurement, changed-paths]", ratchet)
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
        compute = step_block(measurement, "Compute coverage")
        step = re.findall(r"(?m)^        timeout-minutes: (\d+)$", compute)
        self.assertEqual((len(job), len(step)), (1, 1))
        # Preserve the original 1.37x margin against the known 39-minute
        # successful instrumented run, rounding up to whole minutes.
        derived_floor = (39 * 137 + 99) // 100
        self.assertGreaterEqual(int(step[0]), derived_floor)
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
    # The push guard has its own eligibility predicate. The lane jobs carry
    # DOCS_LANE_PREDICATE and skip on a change that touches only Markdown no
    # code names. The coverage jobs also carry SCOPE_PREDICATE and skip on a
    # pull request whose test scope is a crate list (ADR-205). always() admits
    # a job after failed dependencies; it is not such a predicate.
    CONDITIONAL_JOBS = {"automerge-push-guard"}
    FORGIVEN_SKIPS = {"automerge-push-guard"}
    DOCS_LANE_PREDICATE = "needs.changed-paths.outputs.docs_only != 'true'"
    SCOPE_PREDICATE = ("(needs.changed-paths.outputs.test_scope == 'full'"
                       " || needs.changed-paths.outputs.test_scope == '')")
    SCOPED_SKIP_JOBS = {"coverage-measurement", "coverage-ratchet"}
    DOCS_LANE_JOBS = {
        "ci", "khive-py", "kg-editor", "supply-chain", "doc-build",
        "dependency-review", "coverage-measurement", "coverage-ratchet",
        "wasm-parity", "vamana-portability", "minio-blob-compat",
        "msrv-check", "core-only",
    }
    # The Windows compile check is not a gate dependency, so nothing forgives its skip; the lane skips it to
    # save the runner, like the gated lane jobs.
    UNGATED_DOCS_LANE_JOBS = {"check-windows"}

    def setUp(self):
        self.workflow = workflow_text("ci.yml")
        self.gate = indented_block(self.workflow, "ci-gate", 2)
        self.needs = {
            line.strip().removeprefix("- ")
            for line in indented_block(self.gate, "needs", 4).splitlines()
        }
        step = self.gate.split("- name: Check aggregated job results\n", 1)[1]
        self.script = textwrap.dedent(step.split("        run: |\n", 1)[1])

    def run_gate(self, results, docs_only="false", classifier=None, test_scope="full"):
        """test_scope=None leaves the output unset, as when the scope step did not run."""
        needs = {job: {"result": result} for job, result in results.items()}
        if "changed-paths" in needs:
            if classifier is not None:
                needs["changed-paths"]["result"] = classifier
            needs["changed-paths"]["outputs"] = {"docs_only": docs_only}
            if test_scope is not None:
                needs["changed-paths"]["outputs"]["test_scope"] = test_scope
        return subprocess.run(
            ["bash", "-e", "-c", self.script],
            env={**os.environ, "NEEDS": json.dumps(needs)},
            capture_output=True, text=True, timeout=5, check=False,
        )

    def job_conditions(self, job):
        return re.findall(r"(?m)^    if: (.+)$", indented_block(self.workflow, job, 2))

    def lane_conditions(self, job):
        predicate = self.DOCS_LANE_PREDICATE
        if job in self.SCOPED_SKIP_JOBS:
            predicate = f"{predicate} && {self.SCOPE_PREDICATE}"
        return [[predicate], [f"always() && {predicate}"]]

    def test_docs_lane_jobs_and_the_gate_list_are_one_set(self):
        # Every job in the workflow, not only the gate's dependencies: a job that takes the predicate and is
        # missing from the gate's needs would otherwise be invisible here.
        jobs = re.findall(r"(?m)^  ([a-z][a-z0-9-]*):$", self.workflow.split("jobs:\n", 1)[1])
        self.assertIn("ci-gate", jobs)
        carrying = {
            job for job in jobs
            if any(self.DOCS_LANE_PREDICATE in condition for condition in self.job_conditions(job))
        }
        self.assertEqual(carrying, self.DOCS_LANE_JOBS | self.UNGATED_DOCS_LANE_JOBS)
        self.assertEqual(carrying & self.needs, self.DOCS_LANE_JOBS)
        self.assertEqual(carrying - self.needs, self.UNGATED_DOCS_LANE_JOBS)
        lists = [set(re.findall(r'"([a-z0-9-]+)"', block.split("]", 1)[0]))
                 for block in self.script.split("then [")[1:]]
        self.assertEqual(lists, [self.DOCS_LANE_JOBS, self.SCOPED_SKIP_JOBS])
        scoped = {
            job for job in jobs
            if any(self.SCOPE_PREDICATE in condition for condition in self.job_conditions(job))
        }
        self.assertEqual(scoped, self.SCOPED_SKIP_JOBS)
        for job in sorted(self.DOCS_LANE_JOBS | self.UNGATED_DOCS_LANE_JOBS):
            with self.subTest(job=job):
                self.assertIn(self.job_conditions(job), self.lane_conditions(job))
                needs_line = re.search(r"(?m)^    needs: (.+)$", indented_block(self.workflow, job, 2))
                self.assertIn("changed-paths", needs_line.group(1))
        for job in ["changed-paths", "docs", "secret-scan", "data-leak-guard", "marketplace",
                    "resolve-revision", "ci-gate"]:
            with self.subTest(kept=job):
                self.assertNotIn(self.DOCS_LANE_PREDICATE, indented_block(self.workflow, job, 2))
        self.assertIn("- changed-paths", indented_block(self.gate, "needs", 4))

    def test_failed_classifier_does_not_forgive_the_lane_skips(self):
        # A failed classifier already fails the gate by itself; the rejected list shows
        # that the lane skips are rejected too rather than hidden behind it.
        results = dict.fromkeys(self.needs, "success")
        results["ci"] = "skipped"
        result = self.run_gate(results, docs_only="true", classifier="failure")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        rejected = result.stdout.split("Gate failure — jobs not green: ", 1)[1].splitlines()
        self.assertEqual(sorted(rejected), ["changed-paths", "ci"])

    def test_gate_forgives_docs_lane_skips_only_after_a_successful_docs_only_classification(self):
        for job in sorted(self.needs):
            for outcome in ("success", "skipped", "failure", "cancelled"):
                for docs_only, test_scope, classifier in (
                        ("true", None, "success"), ("false", "full", "success"),
                        ("true", None, "failure"), ("true", None, "skipped"),
                        ("TRUE", "full", "success"), ("", "full", "success"),
                        ("false", "khive-hnsw kkernel", "success"), ("false", "khive-hnsw", "failure"),
                        ("false", "khive-hnsw", "skipped"), ("false", "FULL", "success"),
                        ("false", "", "success"), ("false", None, "success"), ("", "", "success")):
                    with self.subTest(job=job, outcome=outcome, docs_only=docs_only,
                                      test_scope=test_scope, classifier=classifier):
                        results = dict.fromkeys(self.needs, "success")
                        results[job] = outcome
                        classifier_result = outcome if job == "changed-paths" else classifier
                        result = self.run_gate(results, docs_only=docs_only, classifier=classifier_result,
                                               test_scope=test_scope)
                        lane = classifier_result == "success" and docs_only == "true"
                        scoped = classifier_result == "success" and test_scope not in (None, "", "full", "FULL")
                        accepted = classifier_result == "success" and (outcome == "success" or (
                            outcome == "skipped" and (
                                job in self.FORGIVEN_SKIPS or (lane and job in self.DOCS_LANE_JOBS)
                                or (scoped and job in self.SCOPED_SKIP_JOBS))
                        ))
                        self.assertEqual(
                            result.returncode, 0 if accepted else 1,
                            f"DOCS_LANE_SKIP_POLICY: {job}={outcome}\n" + result.stdout + result.stderr,
                        )

    def test_gate_forgives_skips_only_for_jobs_on_its_allow_list(self):
        conditional = {
            job for job in self.needs
            if re.search(r"(?m)^    if:", indented_block(self.workflow, job, 2))
            and "    if: always()" not in indented_block(self.workflow, job, 2)
            and self.job_conditions(job) not in self.lane_conditions(job)
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
    def test_main_pushes_run_newest_commit_and_cancel_superseded_runs(self):
        concurrency = indented_block(workflow_text("ci.yml"), "concurrency", 0)
        self.assertEqual(
            mapping_entries(concurrency),
            {
                "group: ci-${{ github.workflow }}-${{ "
                "github.event_name == 'pull_request' && github.ref || "
                "github.event_name == 'push' && github.ref == 'refs/heads/main' "
                "&& github.ref || github.run_id }}",
                "cancel-in-progress: ${{ github.event_name == 'pull_request' || "
                "(github.event_name == 'push' && github.ref == 'refs/heads/main') }}",
            },
            "Main pushes must share one group in which the newest push cancels "
            "the running run and replaces the waiting one, so a burst of merges "
            "leaves one main run at the newest head; PRs retain ref-scoped "
            "cancellation, and other events retain independent run groups.",
        )


def steps_before_checkout_in_missing_directory(text: str) -> list[str]:
    """Name each `run` step that executes before `actions/checkout` in a job whose default
    working directory is a relative path, without setting its own working directory.

    Such a step runs in a directory the checkout has not created yet and fails with
    "No such file or directory" before its command starts.
    """
    offenders = []
    jobs = re.split(r"(?m)^  (?=[A-Za-z0-9_-]+:\n)", text.split("\njobs:\n", 1)[1])
    for job in jobs:
        name = job.split(":", 1)[0]
        workdir = re.search(r"(?m)^    defaults:\n      run:\n(?:        .*\n)*?        working-directory: (\S+)", job)
        if not workdir or workdir.group(1).startswith(("/", "${{")):
            continue
        steps = job.split("\n    steps:\n", 1)
        if len(steps) < 2:
            continue
        for step in re.split(r"(?m)^      - ", steps[1])[1:]:
            if re.search(r"uses: actions/checkout@", step):
                break
            if re.search(r"(?m)^(?:        )?run:", step) and "working-directory:" not in step:
                offenders.append(f"{name}: {step.splitlines()[0].strip()}")
    return offenders


class StepOrderWorkflowTests(unittest.TestCase):
    def test_no_run_step_precedes_checkout_in_a_missing_working_directory(self):
        names = sorted(path.name for path in WORKFLOWS.glob("*.yml"))
        self.assertIn("ci.yml", names)
        offenders = {name: steps_before_checkout_in_missing_directory(workflow_text(name)) for name in names}
        self.assertEqual({name: found for name, found in offenders.items() if found}, {})

    def test_the_check_finds_a_run_step_before_checkout(self):
        text = textwrap.dedent(
            """\
            on: push
            jobs:
              build:
                runs-on: ubuntu-latest
                defaults:
                  run:
                    working-directory: crates
                steps:
                  - name: Too early
                    run: echo early
                  - uses: actions/checkout@v7
                  - name: After
                    run: echo late
              other:
                runs-on: ubuntu-latest
                steps:
                  - name: No default directory
                    run: echo fine
                  - uses: actions/checkout@v7
            """
        )
        self.assertEqual(steps_before_checkout_in_missing_directory(text), ["build: name: Too early"])


class CiMatrixWorkflowTests(unittest.TestCase):
    """Evaluates the `ci` job's matrix expression for each event shape.

    The expression uses only `&&`, `||`, `==`, `startsWith` and string literals,
    which map onto Python's short-circuit operators with the same operand-returning
    semantics, so the test runs the expression itself rather than matching its text.
    """

    UBUNTU = [{"os": "ubuntu-latest", "shard": 1}, {"os": "ubuntu-latest", "shard": 2}]

    def matrix_for(self, event_name, head_ref="", head_repo=None, repository="ohdearquant/khive"):
        text = workflow_text("ci.yml")
        match = re.search(r"include: \$\{\{ fromJSON\((.*?)\) \}\}", text, re.S)
        self.assertIsNotNone(match, "the ci job's matrix include expression was not found")
        expression = " ".join(match.group(1).split())
        names = {
            "github.event.pull_request.head.repo.full_name": "head_repo",
            "github.event_name": "event_name",
            "github.head_ref": "head_ref",
            "github.repository": "repository",
        }
        for name, local in names.items():
            expression = expression.replace(name, local)
        expression = expression.replace("startsWith(", "starts_with(")
        expression = expression.replace("&&", " and ").replace("||", " or ")
        self.assertNotRegex(expression, r"github\.", "an unmapped context name is left in the expression")

        def starts_with(value, prefix):
            # GitHub's startsWith is case-insensitive and treats null as "".
            return (value or "").lower().startswith(prefix.lower())

        scope = {
            "starts_with": starts_with,
            "event_name": event_name,
            "head_ref": head_ref,
            "head_repo": head_repo,
            "repository": repository,
        }
        return json.loads(eval(expression, {"__builtins__": {}}, scope))

    def test_own_codex_split_and_docs_branch_pull_requests_run_the_linux_shards_only(self):
        for head_ref in ["codex/some-change", "split/khive-runtime-daemon-3943", "docs/retrieval-crate-doc"]:
            with self.subTest(head_ref=head_ref):
                self.assertEqual(
                    self.matrix_for("pull_request", head_ref, "ohdearquant/khive"),
                    self.UBUNTU,
                )

    def test_other_pull_requests_keep_the_macos_lane(self):
        expected = self.UBUNTU + [{"os": "macos-latest", "shard": 1}]
        for head_ref, head_repo in [
            ("batch/125", "ohdearquant/khive"),
            ("feature/codex/x", "ohdearquant/khive"),
            ("feature/split/x", "ohdearquant/khive"),
            ("splitter/x", "ohdearquant/khive"),
            ("documents/x", "ohdearquant/khive"),
            ("codex/some-change", "contributor/khive"),
            ("codex/some-change", None),
            ("split/some-change", "contributor/khive"),
            ("docs/some-change", "contributor/khive"),
            ("docs/some-change", None),
        ]:
            with self.subTest(head_ref=head_ref, head_repo=head_repo):
                self.assertEqual(self.matrix_for("pull_request", head_ref, head_repo), expected)

    def test_push_schedule_and_dispatch_runs_keep_all_four_jobs(self):
        expected = self.UBUNTU + [
            {"os": "macos-latest", "shard": 1},
            {"os": "macos-latest", "shard": 2},
        ]
        for event_name in ["push", "schedule", "workflow_dispatch"]:
            with self.subTest(event_name=event_name):
                self.assertEqual(self.matrix_for(event_name), expected)


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
    def test_trend_points_are_nightly_and_per_release_not_per_merge(self):
        on = indented_block(workflow_text("bench-track.yml"), "on", 0)
        self.assertRegex(on, r'(?m)^  schedule:\n    - cron: "[^"]+"$')
        self.assertRegex(on, r"(?m)^  workflow_dispatch:$")
        push = indented_block(on, "push", 2)
        # The one branch push is a merge train's integration branch, which runs the train's dependents
        # closure and records no trend point (ADR-206 Amendment 1); a push to main never runs it.
        self.assertEqual(re.findall(r"(?m)^    branches: (.+)$", push), ['["integration/train-*"]'])
        self.assertIn('tags: ["v*"]', push)

    def test_publish_failure_is_advisory_and_records_incident(self):
        for workflow_name, publish_name in (
            ("bench-component.yml", "Publish ledger to perf-data branch"),
            ("bench-track.yml", "Publish ledgers to perf-data branch"),
        ):
            with self.subTest(workflow=workflow_name):
                workflow = workflow_text(workflow_name)

                def step(name):
                    return workflow.split(f"      - name: {name}\n", 1)[1].split("      - name:", 1)[0]

                publish = step(publish_name)
                warning = step("Warn if trend ledger publish failed (advisory, never fails)")
                self.assertIn("id: publish", publish)
                self.assertIn("continue-on-error: true", publish)
                self.assertIn("steps.publish.outputs.publish_status == 'failed'", warning)
                self.assertIn("steps.publish.outcome == 'failure'", warning)
                artifact = step(
                    "Upload raw Criterion output"
                    if workflow_name == "bench-component.yml"
                    else "Upload raw e2e output"
                )
                for ledger in (
                    ("bench-data/components.jsonl",)
                    if workflow_name == "bench-component.yml"
                    else ("bench-data/pipeline.jsonl", "bench-data/bench-1m.jsonl")
                ):
                    self.assertIn(ledger, artifact)

                with tempfile.TemporaryDirectory() as tmp:
                    root = pathlib.Path(tmp)
                    bin_dir = root / "bin"
                    bin_dir.mkdir()
                    gh = bin_dir / "gh"
                    gh.write_text('#!/bin/sh\nprintf "%s\\n" "$*" >> "$GH_LOG"\n')
                    gh.chmod(0o755)
                    env = {
                        **os.environ,
                        "PATH": f"{bin_dir}:{os.environ['PATH']}",
                        "GH_LOG": str(root / "gh.log"),
                        "GITHUB_STEP_SUMMARY": str(root / "summary"),
                        "GITHUB_SERVER_URL": "https://github.com",
                        "GITHUB_REPOSITORY": "ohdearquant/khive",
                        "GITHUB_RUN_ID": "12345",
                        "COMPONENT": "ann-query",
                    }
                    script = textwrap.dedent(warning.split("        run: |\n", 1)[1])
                    result = subprocess.run(
                        ["bash", "-c", script],
                        cwd=root,
                        env=env,
                        capture_output=True,
                        text=True,
                        check=False,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertIn("issue comment 863", (root / "gh.log").read_text())
                    self.assertIn("perf-data publish failed", (root / "summary").read_text())

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
            self.assertIn("if: ${{ !cancelled() && steps.select.outputs.empty != 'true' }}", step(name))
            self.assertIn("COMPILE_EXIT_CODE: ${{ steps.compile.outputs.exit_code }}", step(name))
            self.assertIn("CRITERION_EXIT_CODE: ${{ steps.criterion.outputs.exit_code }}", step(name))
        self.assertIn("if: always()", step(artifact_name))
        for evidence_path in (
            "component-phases.log", "crates/target/criterion", "bench-data/components.jsonl",
        ):
            self.assertIn(evidence_path, step(artifact_name))
        publish_if = step(publish_name)
        self.assertIn("github.event_name != 'pull_request'", publish_if)
        self.assertIn("github.ref == 'refs/heads/main' || startsWith(github.ref, 'refs/tags/v')", publish_if)

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

    def test_every_benchmarked_library_stays_out_of_cargo_bench(self):
        # `cargo bench --benches` also runs every target whose `bench` flag is on, and a library's defaults to on.
        # Its libtest harness rejects the Criterion flags the quick profile passes (`--quick`, `--noplot`), so a
        # benchmarked crate whose library is not `bench = false` fails the whole component run.
        import tomllib

        benchmarked, offenders = [], []
        for manifest in sorted((REPO_ROOT / "crates").glob("*/Cargo.toml")):
            data = tomllib.loads(manifest.read_text())
            if "package" not in data:
                continue
            auto = data["package"].get("autobenches", True) and list((manifest.parent / "benches").glob("*.rs"))
            if not (data.get("bench") or auto):
                continue
            benchmarked.append(manifest.parent.name)
            has_lib = "lib" in data or (manifest.parent / "src" / "lib.rs").exists()
            if has_lib and data.get("lib", {}).get("bench", True) is not False:
                offenders.append(manifest.parent.name)
        self.assertTrue(benchmarked, "no benchmarked crate found: the manifest scan read nothing")
        self.assertEqual(offenders, [], "set `[lib] bench = false` in these crates' Cargo.toml")

    SELECT_NAME = "Select the component's crates that have a bench target"

    def component_step(self, name):
        # Up to the next step of any shape: the select step is followed by a `uses:` step, not a named one.
        return workflow_text("bench-component.yml").split(f"      - name: {name}\n", 1)[1].split("\n      - ", 1)[0] + "\n"

    def test_component_runs_only_crates_that_have_a_bench_target(self):
        # A component may name a crate before its first benchmark lands. The select step reads cargo metadata and
        # passes compile and Criterion only the crates with a bench target; a name outside the workspace fails it.
        workflow = workflow_text("bench-component.yml")
        select = self.component_step(self.SELECT_NAME)
        self.assertIn("id: select", select)
        self.assertIn("COMPONENT_CRATES: ${{ inputs.crates }}", select)
        self.assertEqual(workflow.count("inputs.crates"), 1, "only the select step reads the declared crate list")
        for name in ("Compile-check this component's bench targets",
                     "Run Criterion benches (quick profile, bounded to 10 minutes)"):
            self.assertIn("COMPONENT_CRATES: ${{ steps.select.outputs.crates }}", self.component_step(name))
        order = [workflow.index(marker) for marker in (
            f"      - name: {self.SELECT_NAME}\n", "      - uses: Swatinem/rust-cache@v2\n",
            "      - name: Compile-check this component's bench targets\n")]
        self.assertEqual(order, sorted(order), "select runs before the cache restore and the compile")
        script = textwrap.dedent(select.split("        run: |\n", 1)[1])
        packages = {"khive-alpha": ["lib", "bench"], "khive-beta": ["lib", "test"], "khive-gamma": ["bench", "lib"],
                    "khive-delta": ["lib"]}
        metadata = {"packages": [{"name": name, "targets": [{"kind": [kind]} for kind in kinds]}
                                 for name, kinds in packages.items()]}

        def run(source, crates):
            with tempfile.TemporaryDirectory() as tmp:
                root = pathlib.Path(tmp)
                (root / "crates").mkdir()
                bins = root / "bin"
                bins.mkdir()
                (root / "metadata.json").write_text(json.dumps(metadata))
                for tool, text in {"cargo": '#!/bin/sh\ncat "$FAKE_METADATA"\n',
                                   "python3": f'#!/bin/sh\nexec "{sys.executable}" "$@"\n'}.items():
                    (bins / tool).write_text(text)
                    (bins / tool).chmod(0o755)
                output = root / "output"
                output.write_text("")
                result = subprocess.run(
                    ["bash", "-c", source], cwd=root / "crates", capture_output=True, text=True, timeout=10, check=False,
                    env={**os.environ, "PATH": f"{bins}:{os.environ['PATH']}", "GITHUB_OUTPUT": str(output),
                         "FAKE_METADATA": str(root / "metadata.json"), "COMPONENT": "fixture-component",
                         "COMPONENT_CRATES": crates})
                values = dict(line.split("=", 1) for line in output.read_text().splitlines())
                log = root / "component-phases.log"
                return result, values, log.read_text() if log.exists() else ""

        result, values, log = run(script, "khive-alpha khive-beta khive-gamma")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(values, {"crates": "khive-alpha khive-gamma", "empty": "false"})
        self.assertIn("phase=select selected=khive-alpha,khive-gamma left_out=khive-beta", log)
        self.assertNotIn("::notice::", result.stdout)

        result, values, log = run(script, "khive-beta khive-delta")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(values, {"crates": "", "empty": "true"})
        self.assertIn("::notice::component fixture-component has no crate with a bench target", result.stdout)
        self.assertIn("no ledger record", result.stdout)

        # A name outside the workspace, or no name at all, fails the step and writes no `empty` output, so the
        # record, report and page steps run as for a compile failure.
        for crates in ("khive-alpha khive-omega", ""):
            with self.subTest(crates=crates):
                result, values, _ = run(script, crates)
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn("empty", values)
                self.assertIn("::error::", result.stdout)
        self.assertIn("khive-omega", run(script, "khive-alpha khive-omega")[0].stdout)

        # Must-fail arm: a selector that keeps every named crate hands khive-beta, which has no bench target, to
        # compile and Criterion, and this fixture tells the two apart.
        keep_all = script.replace('if "bench" in kinds[c]', "if True")
        self.assertNotEqual(keep_all, script)
        self.assertNotEqual(run(keep_all, "khive-alpha khive-beta khive-gamma")[1]["crates"], "khive-alpha khive-gamma")

    def test_an_empty_component_records_and_pages_nothing(self):
        # Evaluates each step's `if:` as GitHub does: an expression with no status function is ANDed with success(),
        # and an output of a skipped or failed step reads as the empty string.
        workflow = workflow_text("bench-component.yml")

        def condition(block):
            found = re.findall(r"(?m)^        if: (.+)$", block)
            self.assertEqual(len(found), 1, block)
            return found[0]

        cache = workflow.split("      - uses: Swatinem/rust-cache@v2\n", 1)[1].split("      - ", 1)[0]
        conditions = {
            "cache": condition(cache),
            "compile": condition(self.component_step("Compile-check this component's bench targets")),
            "record": condition(self.component_step("Record component trend ledger entry")),
            "publish": condition(self.component_step("Publish ledger to perf-data branch")),
            "page": condition(self.component_step("Warn if trend ledger record errored (advisory, never fails)")),
            "report": condition(self.component_step("Report benchmark failures after publication")),
        }

        def evaluate(expression, select_ok, empty, record_exit):
            expression = expression.strip()
            if expression.startswith("${{"):
                expression = expression.removeprefix("${{").removesuffix("}}").strip()
            implicit = not re.search(r"\b(?:cancelled|always|success|failure)\(\)", expression)
            for name, local in {
                "!cancelled()": "True",
                "steps.select.outputs.empty": "empty",
                "steps.record.outputs.exit_code": "record_exit",
                "github.event_name": "event_name",
                "github.ref": "ref",
                "startsWith(": "starts_with(",
                "&&": " and ",
                "||": " or ",
            }.items():
                expression = expression.replace(name, local)
            self.assertNotRegex(expression, r"github\.|steps\.|!(?!=)", "an unmapped name or operator is left")
            scope = {"empty": empty, "record_exit": record_exit, "event_name": "push", "ref": "refs/heads/main",
                     "starts_with": lambda value, prefix: value.lower().startswith(prefix.lower())}
            value = eval(expression, {"__builtins__": {}}, scope)
            return bool(value) and (select_ok or not implicit)

        # (select step succeeded, its `empty` output, the record step's exit code output) -> steps that run.
        cases = {
            (True, "true", ""): set(),
            (True, "false", "0"): {"cache", "compile", "record", "publish", "report"},
            (True, "false", "1"): {"cache", "compile", "record", "publish", "page", "report"},
            (False, "", "1"): {"record", "publish", "page", "report"},
        }
        for (select_ok, empty, record_exit), expected in cases.items():
            with self.subTest(select_ok=select_ok, empty=empty, record_exit=record_exit):
                ran = {name for name, expression in conditions.items()
                       if evaluate(expression, select_ok, empty, record_exit)}
                self.assertEqual(ran, expected)

        # Must-fail arm: without the select guard the record step runs for an empty component and its error record
        # pages the tracking issue, which is what the guard exists to prevent.
        unguarded = {name: expression.replace(" && steps.select.outputs.empty != 'true'", "")
                     for name, expression in conditions.items() if name in ("record", "page")}
        self.assertNotEqual(unguarded, {name: conditions[name] for name in unguarded})
        self.assertTrue(evaluate(unguarded["record"], True, "true", ""))
        self.assertTrue(evaluate(unguarded["page"], True, "true", ""))


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
            "crates/khive-runtime/src/runtime_tests.rs",
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
        # The documentation lane skips every other job that reads Markdown by content, so the docs job runs
        # those readers: the ADR lints and the repository-wide identifier ban, with the ripgrep it needs.
        self.assertIn("run: scripts/ci.sh adr-lint", docs_job)
        self.assertIn("astral-sh/setup-uv@v5", docs_job)
        install = docs_job.index("run: scripts/ci-install-ripgrep.sh")
        self.assertLess(install, docs_job.index("run: scripts/ci.sh content-sweeps"))

    def test_ripgrep_install_has_one_copy(self):
        workflow = workflow_text("ci.yml")
        self.assertNotIn("apt-get install", workflow)
        ci_job = indented_block(workflow, "ci", 2)
        self.assertIn("run: ../scripts/ci-install-ripgrep.sh", ci_job)
        script = (REPO_ROOT / "scripts" / "ci-install-ripgrep.sh").read_text()
        self.assertIn("apt-get install --yes ripgrep", script)
        self.assertIn("set -euo pipefail", script)

    def test_content_sweeps_phase_runs_the_identifier_ban_by_node_id(self):
        # A node id, not a -k filter: a renamed test fails collection instead of selecting nothing.
        script = (REPO_ROOT / "scripts" / "ci.sh").read_text()
        body = script.split("phase_content_sweeps() {", 1)[1].split("\n}\n", 1)[0]
        # The ban reads this file too; a line that names its test file is on its allow-list.
        node = "tests/test_no_importance_in_manifest.py::test_no_importance_identifiers_in_repo"
        self.assertIn(node, body)
        contract_file, function = node.split("::")
        contract = REPO_ROOT / "tests" / "khive-contract" / contract_file
        self.assertIn(f"def {function}(", contract.read_text())

    def test_content_sweeps_phase_runs_the_pack_set_copy_check_by_name(self):
        # This check reads Markdown by content and otherwise runs only in phase_lint, which the
        # documentation lane skips.
        script = (REPO_ROOT / "scripts" / "ci.sh").read_text()
        body = script.split("phase_content_sweeps() {", 1)[1].split("\n}\n", 1)[0]
        self.assertIn('"$SCRIPT_DIR/tests/test_ci_workflows.py"', body)
        self.assertIn("HarnessEnvironmentTests.test_no_new_hand_typed_copy_of_the_shipping_pack_set", body)
        self.assertTrue(callable(getattr(HarnessEnvironmentTests,
                                         "test_no_new_hand_typed_copy_of_the_shipping_pack_set", None)))

    def test_content_sweeps_phase_runs_the_guide_table_checks(self):
        # The directory-map comparison needs only the tree, so it runs in the lane every change runs.
        script = (REPO_ROOT / "scripts" / "ci.sh").read_text()
        body = script.split("phase_content_sweeps() {", 1)[1].split("\n}\n", 1)[0]
        self.assertIn('python3 "$SCRIPT_DIR/../tests/test_documented_guide_tables.py"', body)
        tests = (REPO_ROOT / "tests" / "test_documented_guide_tables.py").read_text()
        self.assertIn("def test_repository_directory_map_matches_the_tree(", tests)

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


class CiPhaseListTests(unittest.TestCase):
    def test_unknown_phase_message_lists_every_dispatched_phase(self):
        script = (REPO_ROOT / "scripts" / "ci.sh").read_text()
        dispatched = re.findall(r"(?m)^        ([a-z-]+)\) phase_", script)
        self.assertIn("adr-lint", dispatched)
        completed = subprocess.run(
            ["sh", str(REPO_ROOT / "scripts" / "ci.sh"), "no-such-phase"],
            cwd=REPO_ROOT, capture_output=True, text=True, timeout=30, check=False,
        )
        self.assertEqual(completed.returncode, 2, completed.stderr)
        listed = re.search(r"(?m)^Valid phases: (.+)$", completed.stderr)
        self.assertIsNotNone(listed, completed.stderr)
        self.assertEqual(listed.group(1).split(), dispatched)


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


class CoreOnlyFeatureWorkflowTests(unittest.TestCase):
    def run_feature_checks(self, failing_feature):
        job = indented_block(workflow_text("ci.yml"), "core-only", 2)
        step = step_block(job, "Check each optional pack independently")
        self.assertIn("if: ${{ !cancelled() }}", step)
        script = textwrap.dedent(step.split("        run: |\n", 1)[1])
        with tempfile.TemporaryDirectory(prefix="core-features-") as directory:
            root = pathlib.Path(directory)
            calls = root / "calls"
            summary = root / "summary"
            cargo = root / "cargo"
            cargo.write_text(
                "#!/bin/sh\n"
                'printf "%s\\n" "$*" >> "$FEATURE_CALLS"\n'
                'case "$*" in *"--features $FAILING_FEATURE "*) exit 19;; esac\n'
            )
            cargo.chmod(0o700)
            env = {key: value for key, value in os.environ.items()
                   if key not in {"BASH_ENV", "ENV"} and not key.startswith("BASH_FUNC_")}
            env.update(PATH=str(root) + os.pathsep + env.get("PATH", ""),
                       FEATURE_CALLS=str(calls), FAILING_FEATURE=failing_feature,
                       GITHUB_STEP_SUMMARY=str(summary))
            result = subprocess.run(
                ["bash", "--noprofile", "--norc", "-eo", "pipefail", "-c", script],
                cwd=root, env=env, text=True, capture_output=True, timeout=10,
                check=False,
            )
            invocations = [shlex.split(line) for line in calls.read_text().splitlines()]
            manifest = (REPO_ROOT / "crates/kkernel/Cargo.toml").read_text()
            feature_table = re.search(r"(?ms)^\[features\]\n(.*?)(?=^\[|\Z)", manifest)
            self.assertIsNotNone(feature_table, "manifest features table exists")
            features = set(re.findall(r"(?m)^(pack-[a-z0-9-]+)\s*=", feature_table.group(1)))
            self.assertTrue(features, "optional pack features are declared")
            self.assertEqual(len(invocations), len(features), "one check per optional pack")
            self.assertEqual(
                {tuple(args) for args in invocations},
                {("check", "-p", "kkernel", "--no-default-features", "--features", name, "--locked")
                 for name in features},
                "each feature compiles independently with defaults disabled",
            )
            return result, summary.read_text()

    def test_all_optional_features_pass_without_enabling_defaults(self):
        result, summary = self.run_feature_checks("not-a-feature")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn(": failed", summary)

    def test_middle_failure_does_not_skip_later_features_or_turn_green(self):
        result, summary = self.run_feature_checks("pack-web")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("pack-web: failed (", summary)
        self.assertIn("pack-moodboard: passed (", summary)

    def test_core_steps_build_and_test_with_defaults_disabled(self):
        job = indented_block(workflow_text("ci.yml"), "core-only", 2)
        expected = {
            "Compile every target without the optional packs": [
                "cargo", "check", "-p", "kkernel", "-p", "khive-mcp",
                "--no-default-features", "--all-targets", "--locked",
            ],
            "Run the library tests without the optional packs": [
                "cargo", "test", "-p", "kkernel", "-p", "khive-mcp",
                "--no-default-features", "--lib", "--locked",
            ],
            "Run the pack composition tests without the optional packs": [
                "cargo", "test", "-p", "kkernel", "--no-default-features", "--locked",
                "--test", "optional_pack_features", "--test", "gate_operation_census",
                "--test", "descriptions_are_written_for_callers",
            ],
        }
        for name, argv in expected.items():
            step = step_block(job, name)
            command = step.split("        run: ", 1)[1].splitlines()[0]
            self.assertEqual(shlex.split(command), argv, name)
        for test in ("optional_pack_features", "gate_operation_census",
                     "descriptions_are_written_for_callers"):
            self.assertTrue((REPO_ROOT / "crates/kkernel/tests" / f"{test}.rs").is_file(), test)

    def test_core_only_job_is_required_by_the_gate(self):
        workflow = workflow_text("ci.yml")
        gate_needs = indented_block(indented_block(workflow, "ci-gate", 2), "needs", 4)
        needs = {line.strip().removeprefix("- ") for line in gate_needs.splitlines() if line.strip()}
        self.assertIn("msrv-check", needs, "the needs list was read")
        self.assertIn("core-only", needs)


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
        "core-only": "Core-only feature set build and tests",
        "coverage-measurement": "Coverage measurement",
        "coverage-ratchet": "Coverage ratchet",
        "changed-paths": "Classify changed paths",
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
        self.env.update(NO_AUTO_MAINTENANCE)

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

    def test_fixture_git_turns_off_auto_maintenance(self):
        self.assertEqual(self.git(self.root, "config", "--get", "maintenance.auto"), "false")

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
            self.assertRegex(job, r"(?m)^    needs: (?:resolve-revision|\[resolve-revision(?:, coverage-measurement)?"
                                  r"(?:, changed-paths)?\])$",
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
        # Two job-level predicates are admitted: the documentation lane, and
        # on the coverage jobs the test scope beside it. A job they skip is
        # reported under its unevaluated name, which is neither a required
        # check name nor an ordinary job name, so the replay distinction holds;
        # CI gate and the secret scan never carry them, and the gate accepts
        # those skips only on a successful docs_only=true classification or,
        # for the coverage jobs, a successful crate-list test scope
        # (AggregateGateWorkflowTests). Gating the same jobs per step would
        # still start every runner, the macOS one included.
        docs_lane = "needs.changed-paths.outputs.docs_only != 'true'"
        scoped_lane = (f"{docs_lane} && (needs.changed-paths.outputs.test_scope == 'full'"
                       " || needs.changed-paths.outputs.test_scope == '')")
        for name in [*self.CHECKOUT_NAMES, "ci-gate"]:
            with self.subTest(job=name):
                job = indented_block(self.workflow, name, 2)
                conditions = re.findall(r"(?m)^    if: (.+)$", job)
                self.assertNotRegex(job, r"(?m)^    if: [>|]",
                                    "CHECK_NAME_ADMISSION: no folded eligibility predicate")
                self.assertIn(conditions, [[], ["always()"], [docs_lane], [f"always() && {docs_lane}"],
                                           [scoped_lane], [f"always() && {scoped_lane}"]],
                              "CHECK_NAME_ADMISSION: eligibility belongs on steps")
                if name == "dependency-review":
                    self.assertEqual(
                        conditions, [f"always() && {docs_lane}"],
                        "CHECK_NAME_ADMISSION: admit even after failed or skipped dependencies",
                    )
                if name == "coverage-ratchet":
                    self.assertEqual(
                        conditions, [f"always() && {scoped_lane}"],
                        "CHECK_NAME_ADMISSION: admit even after failed or skipped dependencies",
                    )
                if name in {"ci-gate", "secret-scan", "docs", "changed-paths", "resolve-revision"}:
                    self.assertNotIn(docs_lane, job)

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


class DocsOnlyClassifierTests(unittest.TestCase):
    """scripts/ci-docs-only.sh on real temporary repositories, and the classifier job's event routing."""

    CLASSIFIER = REPO_ROOT / "scripts" / "ci-docs-only.sh"

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.repo = pathlib.Path(self.temp.name) / "repo"
        self.repo.mkdir()
        self.env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
        self.env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull,
                        GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@example.com",
                        GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@example.com")
        self.env.update(NO_AUTO_MAINTENANCE)
        self.git("init", "-q", "-b", "main")
        self.write("src/lib.rs", "pub fn f() {}\n")
        self.write("notes/guide.md", "# Guide\n")
        self.write("notes/wire.md", "# Wire\n")
        self.write("tests/wire.rs", 'const DOC: &str = include_str!("../notes/wire.md");\n')
        self.write("docs/guide/counts.md", "# Counts\n")
        self.write("docs/adr/ADR-001-guide.md", "# ADR\n")
        self.base = self.commit("base")

    def git(self, *args):
        return subprocess.run(
            ["git", "-c", "core.hooksPath=" + os.devnull, *args], cwd=self.repo, env=self.env,
            text=True, capture_output=True, timeout=10, check=True,
        ).stdout.strip()

    def write(self, path, text):
        target = self.repo / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text)

    def commit(self, message):
        self.git("add", "-A")
        self.git("commit", "-q", "--allow-empty", "-m", message)
        return self.git("rev-parse", "HEAD")

    def classify(self, base, head):
        completed = subprocess.run(
            ["bash", str(self.CLASSIFIER), base, head], cwd=self.repo, env=self.env,
            text=True, capture_output=True, timeout=30, check=False,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn(completed.stdout, ["true\n", "false\n"], completed.stderr)
        return completed.stdout.strip(), completed.stderr

    def test_markdown_no_other_file_names_takes_the_documentation_lane(self):
        self.write("notes/guide.md", "# Guide\n\nMore.\n")
        verdict, reasons = self.classify(self.base, self.commit("guide"))
        self.assertEqual(verdict, "true", reasons)
        self.assertIn("documentation only: notes/guide.md", reasons)

    def test_fixture_commits_start_no_background_maintenance(self):
        # Control: with maintenance kept in the foreground the same commit records the child, so the
        # empty list for the fixture environment is an absence the trace could have shown.
        foreground = {**self.env, "GIT_CONFIG_KEY_0": "maintenance.autoDetach"}
        self.assertNotEqual(commit_maintenance_runs(self.repo, foreground, "control"), [])
        self.assertEqual(commit_maintenance_runs(self.repo, self.env, "fixture"), [])

    def test_markdown_named_by_code_runs_the_full_suite(self):
        self.write("notes/wire.md", "# Wire\n\nChanged.\n")
        verdict, reasons = self.classify(self.base, self.commit("wire"))
        self.assertEqual(verdict, "false")
        self.assertIn("notes/wire.md is named by a non-Markdown file", reasons)

    def test_markdown_the_count_scanner_reads_runs_the_full_suite(self):
        # No file names it, but the smoke tests check its verb and pack counts against the
        # live registry, which only the full suite builds.
        self.write("docs/guide/counts.md", "# Counts\n\nThe kg pack has twenty verbs.\n")
        verdict, reasons = self.classify(self.base, self.commit("counts"))
        self.assertEqual(verdict, "false", reasons)
        self.assertIn("docs/guide/counts.md carries verb or pack counts", reasons)

    def test_an_adr_takes_the_documentation_lane(self):
        self.write("docs/adr/ADR-001-guide.md", "# ADR\n\nAmended.\n")
        verdict, reasons = self.classify(self.base, self.commit("adr"))
        self.assertEqual(verdict, "true", reasons)

    def test_a_failing_count_scanner_runs_the_full_suite(self):
        # A copy of the classifier beside no scanner: the question cannot be answered, so the
        # change takes the full suite rather than the documentation lane.
        self.write("docs/adr/ADR-001-guide.md", "# ADR\n\nAmended.\n")
        head = self.commit("adr")
        (self.repo / "scripts").mkdir(exist_ok=True)
        shutil.copy(self.CLASSIFIER, self.repo / "scripts" / "ci-docs-only.sh")
        completed = subprocess.run(
            ["bash", "scripts/ci-docs-only.sh", self.base, head], cwd=self.repo, env=self.env,
            text=True, capture_output=True, timeout=30, check=False,
        )
        self.assertEqual((completed.returncode, completed.stdout), (0, "false\n"), completed.stderr)
        self.assertIn("count scanner failed for docs/adr/ADR-001-guide.md", completed.stderr)

    def test_code_that_starts_like_a_comment_still_names_markdown(self):
        cases = [
            ("src/attr.rs", '#![doc = include_str!("../notes/attr.md")]\n', "notes/attr.md"),
            ("src/deref.rs", 'fn f(s: &mut &str) {\n    *s = include_str!("../notes/deref.md");\n}\n',
             "notes/deref.md"),
            ("scripts/build.sh", 'render \\\n    --input notes/flag.md\n', "notes/flag.md"),
        ]
        for code_path, code, markdown in cases:
            with self.subTest(code_path=code_path):
                self.write(markdown, "# Notes\n")
                self.write(code_path, code)
                base = self.commit("base " + code_path)
                self.write(markdown, "# Notes\n\nChanged.\n")
                verdict, reasons = self.classify(base, self.commit("change " + markdown))
                self.assertEqual(verdict, "false", reasons)
                self.assertIn(f"{markdown} is named by a non-Markdown file", reasons)

    def test_doc_comments_are_code(self):
        # rustdoc compiles and runs the examples in ///, //!, /** and /*! comments, so a Markdown file they
        # name may be read by a doctest.
        cases = [
            ("src/outer.rs", '/// ```\n/// let s = include_str!("../notes/outer.md");\n/// ```\nfn o() {}\n',
             "notes/outer.md"),
            ("src/inner.rs", '//! let s = include_str!("../notes/inner.md");\n', "notes/inner.md"),
            ("src/block.rs", '/** let s = include_str!("../notes/block.md"); */\nfn b() {}\n', "notes/block.md"),
            ("src/iblock.rs", '/*! let s = include_str!("../notes/iblock.md"); */\n', "notes/iblock.md"),
        ]
        for code_path, code, markdown in cases:
            with self.subTest(code_path=code_path):
                self.write(markdown, "# Notes\n")
                self.write(code_path, code)
                base = self.commit("base " + code_path)
                self.write(markdown, "# Notes\n\nChanged.\n")
                verdict, reasons = self.classify(base, self.commit("change " + markdown))
                self.assertEqual(verdict, "false", reasons)
                self.assertIn(f"{markdown} is named by a non-Markdown file", reasons)

    def test_a_dot_named_markdown_file_follows_the_same_rules(self):
        # The count scanner's globs and its path predicate agree on dot-named paths, so a dot-named file
        # needs no rule of its own: unnamed it takes the lane, named by code it runs the full suite.
        self.write("notes/.hidden.md", "# Hidden\n")
        base = self.commit("hidden")
        self.write("notes/.hidden.md", "# Hidden\n\nChanged.\n")
        verdict, reasons = self.classify(base, self.commit("change hidden"))
        self.assertEqual(verdict, "true", reasons)
        self.write("tests/hidden.rs", 'const H: &str = include_str!("../notes/.hidden.md");\n')
        base = self.commit("name hidden")
        self.write("notes/.hidden.md", "# Hidden\n\nAgain.\n")
        verdict, reasons = self.classify(base, self.commit("change hidden again"))
        self.assertEqual(verdict, "false", reasons)
        self.assertIn("notes/.hidden.md is named by a non-Markdown file", reasons)

    def test_the_scanner_predicate_and_its_globs_agree_on_dot_named_paths(self):
        sys.path.insert(0, str(REPO_ROOT / "tests"))
        try:
            import documented_verb_counts as scanner
        finally:
            sys.path.remove(str(REPO_ROOT / "tests"))
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            paths = ["docs/.hidden.md", "docs/.d/a.md", "marketplace/.m.md", "scripts/.s.md"]
            for rel in paths:
                (root / rel).parent.mkdir(parents=True, exist_ok=True)
                (root / rel).write_text("# x\n")
            globbed = {p.relative_to(root).as_posix() for g in scanner.PUBLISHED_GLOBS for p in root.glob(g)}
            for rel in paths:
                with self.subTest(path=rel):
                    self.assertTrue(scanner.is_published_markdown(rel))
                    self.assertIn(rel, globbed)

    def test_a_name_on_a_line_that_starts_like_a_comment_runs_the_full_suite(self):
        # Plain comments with each marker, then lines that only look like comments: code after a closed
        # /* */, a raw string line, a Python string line and a continued shell argument. A line read on its
        # own cannot tell these apart, so every one of them counts.
        cases = [
            ("tools/check.py", "# see notes/hash.md\nprint(1)\n", "notes/hash.md"),
            ("web/star.js", "/*\n * see notes/star.md\n */\nlet s = 1;\n", "notes/star.md"),
            ("src/line.rs", "// see notes/line.md\nfn l() {}\n", "notes/line.md"),
            ("src/open.rs", "/* see notes/open.md */\nfn p() {}\n", "notes/open.md"),
            ("sql/q.sql", "-- see notes/dash.md\nSELECT 1;\n", "notes/dash.md"),
            ("web/page.html", "<!-- see notes/html.md -->\n<p></p>\n", "notes/html.md"),
            ("src/inline.rs", '/* a */ const D: &str = include_str!("../notes/inline.md");\n', "notes/inline.md"),
            ("src/raw.rs", 'const T: &str = r#"\n// notes/raw.md\n"#;\n', "notes/raw.md"),
            ("tools/doc.py", 'DOCS = """\n# notes/pystr.md\n"""\n', "notes/pystr.md"),
            ("scripts/run.sh", "render \\\n    -- notes/dashdash.md\n", "notes/dashdash.md"),
        ]
        for code_path, code, markdown in cases:
            with self.subTest(code_path=code_path):
                self.write(markdown, "# Notes\n")
                self.write(code_path, code)
                base = self.commit("base " + code_path)
                self.write(markdown, "# Notes\n\nChanged.\n")
                verdict, reasons = self.classify(base, self.commit("change " + markdown))
                self.assertEqual(verdict, "false", reasons)
                self.assertIn(f"{markdown} is named by a non-Markdown file", reasons)

    def test_a_binary_file_that_names_the_markdown_runs_the_full_suite(self):
        fixture = self.repo / "fixtures" / "guide.bin"
        fixture.parent.mkdir(parents=True, exist_ok=True)
        fixture.write_bytes(b"\x00\x01notes/guide.md\x00")
        base = self.commit("binary fixture")
        self.write("notes/guide.md", "# Guide\n\nMore.\n")
        verdict, reasons = self.classify(base, self.commit("guide"))
        self.assertEqual(verdict, "false", reasons)
        self.assertIn("notes/guide.md is named by a non-Markdown file", reasons)

    def test_a_symlink_whose_target_names_the_markdown_runs_the_full_suite(self):
        link = self.repo / "fixtures" / "guide.json"
        link.parent.mkdir(parents=True, exist_ok=True)
        link.symlink_to("../notes/guide.md")
        base = self.commit("symlink")
        self.write("notes/guide.md", "# Guide\n\nMore.\n")
        verdict, reasons = self.classify(base, self.commit("guide"))
        self.assertEqual(verdict, "false", reasons)
        self.assertIn("notes/guide.md is named by a symlink's target", reasons)

    def test_a_symlink_to_another_file_leaves_the_documentation_lane(self):
        link = self.repo / "fixtures" / "lib.rs"
        link.parent.mkdir(parents=True, exist_ok=True)
        link.symlink_to("../src/lib.rs")
        base = self.commit("symlink")
        self.write("notes/guide.md", "# Guide\n\nMore.\n")
        verdict, reasons = self.classify(base, self.commit("guide"))
        self.assertEqual(verdict, "true", reasons)

    def test_any_change_while_the_tree_has_a_submodule_runs_the_full_suite(self):
        # The gitlink has no checkout here, so both commits stage by path: add -A would record its removal.
        self.git("update-index", "--add", "--cacheinfo", f"160000,{self.base},vendor/sub")
        self.git("commit", "-q", "-m", "submodule")
        base = self.git("rev-parse", "HEAD")
        self.write("notes/guide.md", "# Guide\n\nMore.\n")
        self.git("add", "notes/guide.md")
        self.git("commit", "-q", "-m", "guide")
        verdict, reasons = self.classify(base, self.git("rev-parse", "HEAD"))
        self.assertEqual(verdict, "false", reasons)
        self.assertIn("has a submodule, whose files are not searched", reasons)

    def test_any_non_markdown_path_runs_the_full_suite(self):
        self.write("notes/guide.md", "# Guide\n\nMore.\n")
        self.write("src/lib.rs", "pub fn g() {}\n")
        verdict, reasons = self.classify(self.base, self.commit("mixed"))
        self.assertEqual(verdict, "false")
        self.assertIn("not Markdown: src/lib.rs", reasons)

    def test_a_source_file_renamed_to_markdown_runs_the_full_suite(self):
        self.git("mv", "tests/wire.rs", "tests/wire-notes.md")
        verdict, reasons = self.classify(self.base, self.commit("rename"))
        self.assertEqual(verdict, "false")
        self.assertIn("not Markdown: tests/wire.rs", reasons)

    def test_deleting_an_unnamed_markdown_file_takes_the_documentation_lane(self):
        self.git("rm", "-q", "notes/guide.md")
        verdict, reasons = self.classify(self.base, self.commit("delete"))
        self.assertEqual(verdict, "true", reasons)

    def test_no_change_and_an_unknown_revision_run_the_full_suite(self):
        verdict, reasons = self.classify(self.base, self.commit("empty"))
        self.assertEqual(verdict, "false")
        self.assertIn("no changed paths", reasons)
        verdict, reasons = self.classify("0" * 40, self.base)
        self.assertEqual(verdict, "false")
        self.assertIn("git diff", reasons)

    def test_many_code_references_are_still_found(self):
        # Many matching lines make an early-exiting grep -q close its input while the
        # writer is still writing; read through a pipe under pipefail, that would report
        # "not named", the unsafe direction.
        self.write("src/big.rs", 'const G: &str = "notes/guide.md";\n' * 20000)
        base = self.commit("big")
        self.write("notes/guide.md", "# Guide\n\nMore.\n")
        verdict, reasons = self.classify(base, self.commit("guide"))
        self.assertEqual(verdict, "false", reasons)

    def classify_job(self, event_name, head_sha, ref="refs/heads/main", gh_script=None, head_ref=None,
                     head_repository=None, classifier=None):
        """Runs the Classify step and returns its docs_only line. head_ref and head_repository are left
        unset unless given, as on a push. classifier, when given, is the text of the checked-out
        scripts/ci-docs-only.sh in place of this repository's own."""
        workflow = workflow_text("ci.yml")
        job = indented_block(workflow, "changed-paths", 2)
        step = job.split("- name: Classify\n", 1)[1]
        match = re.search(r"(?m)^        run: \|\n((?:          .*\n|\n)+)", step + "\n")
        self.assertIsNotNone(match)
        script = textwrap.dedent(match.group(1))
        (self.repo / "scripts").mkdir(exist_ok=True)
        if classifier is None:
            shutil.copy(self.CLASSIFIER, self.repo / "scripts" / "ci-docs-only.sh")
        else:
            self.write("scripts/ci-docs-only.sh", classifier)
        (self.repo / "tests").mkdir(exist_ok=True)
        shutil.copy(REPO_ROOT / "tests" / "documented_verb_counts.py",
                    self.repo / "tests" / "documented_verb_counts.py")
        bin_dir = pathlib.Path(self.temp.name) / "bin"
        bin_dir.mkdir(exist_ok=True)
        if gh_script is not None:
            gh = bin_dir / "gh"
            gh.write_text("#!/bin/bash\n" + gh_script)
            gh.chmod(0o755)
        output = pathlib.Path(self.temp.name) / "output"
        output.write_text("")
        pull = {key: value for key, value in (("HEAD_REF", head_ref), ("HEAD_REPOSITORY", head_repository))
                if value is not None}
        completed = subprocess.run(
            ["bash", "-e", "-c", script], cwd=self.repo, text=True, capture_output=True, timeout=30,
            env={**self.env, "PATH": f"{bin_dir}:{os.environ['PATH']}", "GITHUB_OUTPUT": str(output),
                 "EVENT_NAME": event_name, "REF": ref, "HEAD_SHA": head_sha,
                 "REPOSITORY": "o/r", "GH_TOKEN": "x", **pull},
            check=False,
        )
        self.assertEqual(completed.returncode, 0, completed.stdout + completed.stderr)
        lines = output.read_text().splitlines()
        # A push that found a fully tested base also records it, for the Scope step, before docs_only.
        self.assertIn([line.split("=", 1)[0] for line in lines], (["docs_only"], ["base", "docs_only"]), lines)
        self.outputs = dict(line.split("=", 1) for line in lines)
        return lines[-1], completed.stdout + completed.stderr

    def merge_ref(self, path, text):
        self.git("checkout", "-q", "-b", "topic")
        self.write(path, text)
        topic = self.commit("topic")
        self.git("checkout", "-q", "main")
        self.write("other.md", "# Other\n")
        self.commit("main moved")
        self.git("merge", "-q", "--no-ff", "-m", "merge", topic)
        return self.git("rev-parse", "HEAD")

    def test_job_pull_request_compares_the_merge_ref_with_its_base_parent(self):
        head = self.merge_ref("notes/guide.md", "# Guide\n\nMore.\n")
        self.assertEqual(self.classify_job("pull_request", head)[0], "docs_only=true")

    def test_job_pull_request_with_code_runs_the_full_suite(self):
        head = self.merge_ref("src/lib.rs", "pub fn h() {}\n")
        self.assertEqual(self.classify_job("pull_request", head)[0], "docs_only=false")

    # A classifier that answers "documentation only" for every change, as a change could carry.
    LENIENT_CLASSIFIER = "#!/bin/bash\necho true\n"
    CLASSIFIER_CHANGED = "full suite: the change edits a path classifier"

    def test_job_pull_request_that_edits_the_docs_classifier_runs_the_full_suite(self):
        head = self.merge_ref("scripts/ci-docs-only.sh", self.LENIENT_CLASSIFIER)
        verdict, log = self.classify_job("pull_request", head, classifier=self.LENIENT_CLASSIFIER)
        self.assertEqual(verdict, "docs_only=false", log)
        self.assertIn(f"{self.CLASSIFIER_CHANGED}: scripts/ci-docs-only.sh\n", log)

    def test_job_pull_request_that_edits_the_scope_script_reports_a_classifier_change(self):
        head = self.merge_ref("scripts/ci-test-scope.py", "print('map')\n")
        verdict, log = self.classify_job("pull_request", head)
        self.assertEqual(verdict, "docs_only=false", log)
        self.assertIn(f"{self.CLASSIFIER_CHANGED}: scripts/ci-test-scope.py\n", log)

    def test_job_pull_request_with_only_documentation_does_not_report_a_classifier_change(self):
        head = self.merge_ref("notes/guide.md", "# Guide\n\nMore.\n")
        verdict, log = self.classify_job("pull_request", head)
        self.assertEqual(verdict, "docs_only=true", log)
        self.assertNotIn(self.CLASSIFIER_CHANGED, log)

    def test_job_integration_push_that_edits_a_classifier_runs_the_full_suite(self):
        self.write("scripts/ci-docs-only.sh", self.LENIENT_CLASSIFIER)
        self.write("notes/guide.md", "# Guide\n\nMore.\n")
        head = self.commit("a lenient classifier beside documentation")
        verdict, log = self.classify_job("push", head, ref="refs/heads/integration/train-1",
                                         gh_script=self.gh_listing(self.base, "success"),
                                         classifier=self.LENIENT_CLASSIFIER)
        self.assertEqual(verdict, "docs_only=false", log)
        self.assertEqual(self.outputs.get("base"), self.base, log)
        self.assertIn(f"{self.CLASSIFIER_CHANGED}: scripts/ci-docs-only.sh\n", log)

    def test_job_own_codex_pull_request_with_crate_source_is_not_a_lane(self):
        # The integration-only lane is retired (ADR-205 D7): the branch name changes nothing.
        head = self.merge_ref("crates/k/src/lib.rs", "pub fn h() {}\n")
        verdict, log = self.classify_job("pull_request", head, head_ref="codex/change", head_repository="o/r")
        self.assertEqual(verdict, "docs_only=false", log)

    def gh_listing(self, run_sha, shard_conclusion, conclusion="success", record="test-scope-full"):
        return self.gh_runs((101, run_sha, conclusion, shard_conclusion, record))

    def gh_runs(self, *runs):
        """A fake gh answering the tested-base lookup: each run is (id, head sha, run conclusion, Linux shard 1
        conclusion, recorded scope artifact name or None), newest first, after the step's own jq."""
        listing = "\\n".join(f"{run_id} {sha} {conclusion}" for run_id, sha, conclusion, _, _ in runs)
        cases = [f'  */workflows/ci.yml/runs*) printf "{listing}\\n" ;;\n']
        for run_id, _, _, shard, record in runs:
            cases.append(f'  */runs/{run_id}/jobs*) echo {1 if shard == "success" else 0} ;;\n')
            cases.append(f'  */runs/{run_id}/artifacts*) echo {1 if record == "test-scope-full" else 0} ;;\n')
        return 'case "$2" in\n' + "".join(cases) + '  *) exit 1 ;;\nesac\n'

    def test_job_main_push_compares_with_the_newest_fully_tested_main_commit(self):
        self.write("src/lib.rs", "pub fn untested() {}\n")
        untested = self.commit("code that replaced a waiting run")
        self.write("notes/guide.md", "# Guide\n\nMore.\n")
        head = self.commit("documentation")
        # Baseline before the untested code commit: the code is in the range, so the full suite runs.
        verdict, log = self.classify_job("push", head, gh_script=self.gh_listing(self.base, "success"))
        self.assertEqual(verdict, "docs_only=false", log)
        # Baseline at the untested commit itself: only Markdown since then.
        verdict, log = self.classify_job("push", head, gh_script=self.gh_listing(untested, "success"))
        self.assertEqual(verdict, "docs_only=true", log)
        self.assertIn(f"Compared with {untested}", log)

    def test_job_main_push_fetches_main_history_into_a_one_commit_checkout(self):
        self.write("src/lib.rs", "pub fn untested() {}\n")
        untested = self.commit("code that replaced a waiting run")
        self.write("notes/guide.md", "# Guide\n\nMore.\n")
        head = self.commit("documentation")
        # The runner checks out the pushed commit alone from a remote that holds main's history.
        remote = pathlib.Path(self.temp.name) / "remote.git"
        self.git("clone", "-q", "--bare", str(self.repo), str(remote))
        self.git("-C", str(remote), "config", "uploadpack.allowFilter", "true")
        checkout = pathlib.Path(self.temp.name) / "checkout"
        self.git("clone", "-q", "--depth", "1", "--no-tags", remote.as_uri(), str(checkout))
        self.repo = checkout
        self.assertEqual(self.git("rev-parse", "--is-shallow-repository"), "true")
        self.assertEqual(self.git("rev-parse", "HEAD"), head)
        verdict, log = self.classify_job("push", head, gh_script=self.gh_listing(untested, "success"))
        self.assertEqual(verdict, "docs_only=true", log)
        self.assertIn(f"Compared with {untested}", log)
        self.assertEqual(self.git("rev-parse", "--is-shallow-repository"), "false")

    def test_job_main_push_without_a_full_run_or_with_a_failing_api_runs_the_full_suite(self):
        self.write("notes/guide.md", "# Guide\n\nMore.\n")
        head = self.commit("documentation")
        for gh_script in (self.gh_listing(self.base, "skipped"), "exit 1\n"):
            with self.subTest(gh_script=gh_script):
                verdict, log = self.classify_job("push", head, gh_script=gh_script)
                self.assertEqual(verdict, "docs_only=false", log)
                self.assertIn("no main commit with a completed full run", log)

    def test_job_other_events_run_the_full_suite(self):
        self.write("notes/guide.md", "# Guide\n\nMore.\n")
        head = self.commit("documentation")
        for event_name, ref in (("schedule", "refs/heads/main"), ("workflow_dispatch", "refs/heads/main"),
                                ("push", "refs/heads/release/v1")):
            with self.subTest(event_name=event_name, ref=ref):
                self.assertEqual(self.classify_job(event_name, head, ref=ref)[0], "docs_only=false")
                self.assertNotIn("base", self.outputs)

    def test_job_integration_push_compares_with_the_newest_fully_tested_main_commit(self):
        train = "refs/heads/integration/train-1"
        self.write("src/lib.rs", "pub fn untested() {}\n")
        untested = self.commit("code that a train carries")
        self.write("notes/guide.md", "# Guide\n\nMore.\n")
        head = self.commit("documentation in the same train")
        verdict, log = self.classify_job("push", head, ref=train, gh_script=self.gh_listing(self.base, "success"))
        self.assertEqual(verdict, "docs_only=false", log)
        self.assertEqual(self.outputs.get("base"), self.base, log)
        verdict, log = self.classify_job("push", head, ref=train, gh_script=self.gh_listing(untested, "success"))
        self.assertEqual(verdict, "docs_only=true", log)
        self.assertEqual(self.outputs.get("base"), untested, log)
        self.assertIn(f"Compared with {untested}", log)

    def test_job_integration_push_fetches_main_and_its_own_branch_into_a_one_commit_checkout(self):
        self.write("src/lib.rs", "pub fn tested() {}\n")
        tested = self.commit("main's newest fully tested commit")
        self.git("checkout", "-q", "-b", "integration/train-1")
        self.write("notes/guide.md", "# Guide\n\nMore.\n")
        head = self.commit("a documentation-only train")
        self.git("checkout", "-q", "main")
        remote = pathlib.Path(self.temp.name) / "remote.git"
        self.git("clone", "-q", "--bare", str(self.repo), str(remote))
        self.git("-C", str(remote), "config", "uploadpack.allowFilter", "true")
        checkout = pathlib.Path(self.temp.name) / "checkout"
        self.git("clone", "-q", "--depth", "1", "--no-tags", "-b", "integration/train-1", remote.as_uri(),
                 str(checkout))
        self.repo = checkout
        self.assertEqual(self.git("rev-parse", "--is-shallow-repository"), "true")
        self.assertEqual(self.git("rev-parse", "HEAD"), head)
        verdict, log = self.classify_job("push", head, ref="refs/heads/integration/train-1",
                                         gh_script=self.gh_listing(tested, "success"))
        self.assertEqual(verdict, "docs_only=true", log)
        self.assertEqual(self.outputs.get("base"), tested, log)
        self.assertEqual(self.git("rev-parse", "--is-shallow-repository"), "false")

    def test_job_checks_out_integration_pushes(self):
        job = indented_block(workflow_text("ci.yml"), "changed-paths", 2)
        checkout = job.split("- uses: actions/checkout@", 1)[1].split("\n      - ", 1)[0]
        condition = re.search(r"(?m)^        if: (.+)$", checkout).group(1)
        self.assertIn("startsWith(github.ref, 'refs/heads/integration/')", condition)
        self.assertIn("github.ref == 'refs/heads/main'", condition)

    def test_job_tested_base_skips_a_failed_run_an_unrecorded_run_and_a_scoped_run(self):
        # ADR-205 Amendment 1, verify-by 1: the newest candidate is unusable, an older one is the base. If the
        # newest were used, only Markdown would follow it and the push would take the documentation lane.
        self.write("src/lib.rs", "pub fn untested() {}\n")
        untested = self.commit("code that a train carries")
        self.write("notes/guide.md", "# Guide\n\nMore.\n")
        head = self.commit("documentation")
        for label, newest in (("run concluded with a failure", (102, untested, "failure", "success", "test-scope-full")),
                              ("scope recorded as scoped", (102, untested, "success", "success", "test-scope-scoped")),
                              ("no scope record", (102, untested, "success", "success", None)),
                              ("shard 1 failed", (102, untested, "success", "failure", "test-scope-full"))):
            for ref in ("refs/heads/integration/train-1", "refs/heads/main"):
                with self.subTest(label=label, ref=ref):
                    gh = self.gh_runs(newest, (101, self.base, "success", "success", "test-scope-full"))
                    verdict, log = self.classify_job("push", head, ref=ref, gh_script=gh)
                    self.assertEqual(verdict, "docs_only=false", log)
                    self.assertEqual(self.outputs.get("base"), self.base, log)
        # control: the same newest candidate, usable, is the base
        gh = self.gh_runs((102, untested, "success", "success", "test-scope-full"),
                          (101, self.base, "success", "success", "test-scope-full"))
        verdict, log = self.classify_job("push", head, ref="refs/heads/integration/train-1", gh_script=gh)
        self.assertEqual(verdict, "docs_only=true", log)
        self.assertEqual(self.outputs.get("base"), untested, log)

    def record_step(self, event_name, docs_only, test_scope):
        """Runs the Record step and returns (recorded, the record file's text)."""
        job = indented_block(workflow_text("ci.yml"), "changed-paths", 2)
        step = job.split("- name: Record the test scope\n", 1)[1].split("\n      - name: ", 1)[0]
        self.assertIn("if: github.event_name != 'pull_request'", step)
        script = textwrap.dedent(re.search(r"(?m)^        run: \|\n((?:          .*\n|\n)+)", step + "\n").group(1))
        temp = pathlib.Path(self.temp.name) / f"record-{docs_only}-{test_scope}"
        temp.mkdir()
        output = temp / "output"
        output.write_text("")
        env = {**self.env, "RUNNER_TEMP": str(temp), "GITHUB_OUTPUT": str(output), "DOCS_ONLY": docs_only}
        # The CI job exports its own TEST_SCOPE, so the unset arm removes it rather than inheriting it.
        env.pop("TEST_SCOPE", None)
        if test_scope is not None:
            env["TEST_SCOPE"] = test_scope
        subprocess.run(["bash", "-c", script], env=env, check=True, capture_output=True, text=True, timeout=30)
        lines = output.read_text().splitlines()
        self.assertEqual(len(lines), 1, lines)
        return lines[0].split("=", 1)[1], (temp / "test-scope" / "test_scope").read_text()

    def test_record_step_names_the_scope_the_suites_ran(self):
        for docs_only, test_scope, recorded, text in (("false", "full", "full", "full\n"),
                                                      ("false", "", "full", "full\n"),
                                                      ("false", None, "full", "full\n"),
                                                      ("false", "j k", "scoped", "j k\n"),
                                                      ("true", None, "docs", "docs\n"),
                                                      ("true", "", "docs", "docs\n")):
            with self.subTest(docs_only=docs_only, test_scope=test_scope):
                self.assertEqual(self.record_step("push", docs_only, test_scope), (recorded, text))
        job = indented_block(workflow_text("ci.yml"), "changed-paths", 2)
        upload = job.split("- name: Upload the test scope record\n", 1)[1].split("\n      - name: ", 1)[0]
        self.assertIn("name: test-scope-${{ steps.record.outputs.recorded }}", upload)
        self.assertIn("if: github.event_name != 'pull_request'", upload)
        self.assertIn("continue-on-error: true", upload)
        # the lookup reads exactly the name a full run records
        classify = job.split("- name: Classify\n", 1)[1].split("\n      - name: ", 1)[0]
        self.assertIn('select(.name == "test-scope-full")', classify)

    def test_job_integration_push_without_a_full_run_runs_the_full_suite(self):
        self.write("notes/guide.md", "# Guide\n\nMore.\n")
        head = self.commit("documentation")
        for gh_script in (self.gh_listing(self.base, "skipped"), "exit 1\n"):
            with self.subTest(gh_script=gh_script):
                verdict, log = self.classify_job("push", head, ref="refs/heads/integration/train-1",
                                                 gh_script=gh_script)
                self.assertEqual(verdict, "docs_only=false", log)
                self.assertNotIn("base", self.outputs)
                self.assertIn("no main commit with a completed full run", log)


FAKE_KKERNEL = r'''#!{python}
"""A stand-in for `kkernel exec --strict <ops>` answering the three code-map verbs from graph.json."""
import json, pathlib, re, sys

here = pathlib.Path(__file__).resolve().parent
spec = json.loads((here / "graph.json").read_text())
ops = sys.argv[-1]
with open(here / "calls.log", "a") as log:
    log.write(ops + "\n")
ingested = re.search(r'code\.ingest\(path="([^"]*)"', ops)
if ingested:
    # what the binary was handed: every manifest under the ingested directory, as it read them
    root = pathlib.Path(ingested.group(1))
    (here / "ingested.json").write_text(json.dumps({"path": str(root), "manifests": {
        str(p.relative_to(root)): p.read_text() for p in sorted(root.rglob("Cargo.toml"))}}))
if spec.get("exit"):
    print(json.dumps({"results": [{"ok": False, "error": {"message": "refused"}}]}))
    sys.exit(spec["exit"])
results = []
for verb, args in re.findall(r"code\.(\w+)\(([^)]*)\)", ops):
    if verb == "ingest":
        report = spec.get("ingest", {"projects_created": len(spec["projects"]), "edges_created": len(spec["edges"])})
        results.append({"ok": True, "result": report})
    elif verb == "list":
        cursor = re.search(r'cursor="([^"]*)"', args)
        pages = spec.get("pages") or [sorted(spec["projects"])]
        index = int(cursor.group(1)) if cursor else 0
        items = [{"id": spec["projects"][name], "kind": spec.get("list_kind", "project"), "name": name,
                  "project_id": spec["projects"][name]} for name in pages[index]]
        following = str(index + 1) if index + 1 < len(pages) else None
        results.append({"ok": True, "result": {"items": items, "next_cursor": following}})
    elif verb == "deps":
        ident = re.search(r'id="([^"]*)"', args).group(1)
        edges = [{"source": source, "target": target} for source, target in spec["edges"] if target == ident]
        kinds = spec.get("kinds", {})
        nodes = [{"id": node, "kind": kinds.get(node, "project")}
                 for node in sorted({ident} | {edge["source"] for edge in edges})]
        results.append({"ok": True, "result": {"map": "workspace", "root": ident, "nodes": nodes,
                                               "edges": edges, "truncated": spec.get("truncated", False)}})
    else:
        results.append({"ok": False, "error": {"message": "unknown verb " + verb}})
print(json.dumps({"results": results}))
'''


class ScopeFixture(unittest.TestCase):
    """A temporary repository with three crates and a nested one, and a fake kkernel that serves its map."""

    TREE = {
        "crates/Cargo.toml": "[workspace]\nmembers = [\"k\", \"k/sub\", \"j\", \"m\"]\n",
        "crates/Cargo.lock": "version = 4\n",
        "crates/k/Cargo.toml": "[package]\nname = \"k\"\n",
        "crates/k/src/lib.rs": "pub fn f() {}\n",
        "crates/k/build.rs": "fn main() {}\n",
        "crates/k/tests/t.rs": "#[test]\nfn t() {}\n",
        "crates/k/README.md": "# k\n",
        "crates/k/Cargo.lock": "version = 4\n",
        "crates/k/clippy.toml": "msrv = \"1.85\"\n",
        "crates/k/sub/Cargo.toml": "[package]\nname = \"ksub\"\n",
        "crates/k/sub/src/lib.rs": "pub fn s() {}\n",
        "crates/j/Cargo.toml": "[package]\nname = \"j\"\n",
        "crates/j/src/lib.rs": "pub fn g() {}\n",
        "crates/m/Cargo.toml": "[package]\nname = \"m\"\n",
        "crates/m/src/lib.rs": "pub fn h() {}\n",
        # m compiles a file of k, and j reads a file of m, by naming the path; neither is a dependency edge.
        "crates/k/src/shared.rs": "pub fn shared() {}\n",
        "crates/m/tests/reader.rs": "#[path = \"../../k/src/shared.rs\"]\nmod shared;\n",
        "crates/m/src/table.txt": "rows\n",
        "crates/j/src/census.rs": "pub const S: &str = include_str!(\"../../m/src/table.txt\");\n",
        # an include inside its own crate is not a foreign reader
        "crates/k/tests/own.rs": "#[path = \"../src/lib.rs\"]\nmod own;\n",
        # j reads files of m through each argument form the scan resolves: a literal on the line after the opener,
        # a raw string, concat! led by the manifest directory, and a literal prefix with a computed tail (a partial
        # file name, so only the prefix rule can match it).
        "crates/m/src/multi.txt": "rows\n",
        "crates/j/src/multi.rs": "pub const M: &str = include_str!(\n    \"../../m/src/multi.txt\"\n);\n",
        "crates/m/src/raw.txt": "rows\n",
        "crates/j/src/raw.rs": "pub const R: &str = include_str!(r#\"../../m/src/raw.txt\"#);\n",
        "crates/m/src/env.txt": "rows\n",
        "crates/j/tests/env.rs": "const E: &str = include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/../m/src/env.txt\"));\n",
        "crates/m/sql/q.sql": "select 1;\n",
        "crates/j/src/prefix.rs": "macro_rules! q {\n    ($n:literal) => {\n        include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/../m/sql/q\", $n, \".sql\"))\n    };\n}\n",
        # the same names inside a string literal and comments are not include sites
        "crates/m/src/ghost.txt": "rows\n",
        "crates/k/src/fixture.rs": "pub const F: &str = \"include_str!(\\\"../../m/src/ghost.txt\\\")\";\n// include!(\"../../m/src/ghost.txt\")\n/* #[path = \"../../m/src/ghost.txt\"] */\n",
        "crates/.cargo/config.toml": "[build]\n",
        "crates/.config/nextest.toml": "[profile.default]\n",
        "crates/rust-toolchain.toml": "[toolchain]\n",
        "crates/deny.toml": "[bans]\n",
        "crates/rustfmt.toml": "edition = \"2021\"\n",
        "notes/k-notes.md": "# Notes\n",
        "docs/diagram.svg": "<svg/>\n",
        ".github/workflows/ci.yml": "name: CI\n",
        ".github/actions/a/action.yml": "name: a\n",
        "scripts/ci.sh": "echo ci\n",
        "python/pkg/__init__.py": "\n",
        "tests/contract.py": "\n",
        "benches/b.rs": "\n",
        "apps/a/index.ts": "\n",
        "Makefile": "ci:\n",
        "rust-toolchain.toml": "[toolchain]\n",
    }
    # Paths the path rules admit to the map; every other path in TREE runs the full suite by itself.
    CRATE_SOURCE = {"crates/k/src/lib.rs", "crates/k/build.rs", "crates/k/tests/t.rs",
                    "crates/k/sub/src/lib.rs", "crates/j/src/lib.rs", "crates/m/src/lib.rs",
                    "crates/m/tests/reader.rs", "crates/j/src/census.rs", "crates/k/tests/own.rs",
                    "crates/k/src/shared.rs", "crates/m/src/table.txt", "crates/j/src/multi.rs",
                    "crates/m/src/multi.txt", "crates/j/src/raw.rs", "crates/m/src/raw.txt", "crates/j/tests/env.rs",
                    "crates/m/src/env.txt", "crates/j/src/prefix.rs", "crates/m/sql/q.sql", "crates/m/src/ghost.txt",
                    "crates/k/src/fixture.rs"}
    # Crate source that a reader in another crate names: the path rules admit it, the include scan sends it to full.
    NAMED_ELSEWHERE = {"crates/k/src/shared.rs", "crates/m/src/table.txt", "crates/m/src/multi.txt",
                       "crates/m/src/raw.txt", "crates/m/src/env.txt", "crates/m/sql/q.sql"}
    # j depends on k, and k on ksub.
    GRAPH = {"projects": {"k": "p-k", "ksub": "p-ksub", "j": "p-j", "m": "p-m"},
             "edges": [["p-j", "p-k"], ["p-k", "p-ksub"]]}

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = pathlib.Path(self.temp.name).resolve()
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self.env = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
        self.env.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull,
                        GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@example.com",
                        GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@example.com")
        self.env.update(NO_AUTO_MAINTENANCE)
        self.git("init", "-q", "-b", "main")
        for path, text in self.TREE.items():
            self.write(path, text)
        shutil.copy(REPO_ROOT / "scripts" / "ci-test-scope.py", self.repo / "scripts" / "ci-test-scope.py")
        shutil.copy(REPO_ROOT / "tests" / "documented_verb_counts.py",
                    self.repo / "tests" / "documented_verb_counts.py")
        self.base = self.commit("base")
        self.fake = self.root / "fake"
        self.fake.mkdir()
        kernel = self.fake / "kkernel"
        kernel.write_text(FAKE_KKERNEL.replace("{python}", sys.executable, 1))
        kernel.chmod(0o755)
        self.set_graph(self.GRAPH)

    def set_graph(self, graph):
        (self.fake / "graph.json").write_text(json.dumps(graph))

    def calls(self, directory=None):
        log = (directory or self.fake) / "calls.log"
        return log.read_text().splitlines() if log.exists() else []

    def git(self, *args):
        return subprocess.run(
            ["git", "-c", "core.hooksPath=" + os.devnull, *args], cwd=self.repo, env=self.env,
            text=True, capture_output=True, timeout=10, check=True,
        ).stdout.strip()

    def write(self, path, text):
        target = self.repo / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text)

    def commit(self, message):
        self.git("add", "-A")
        self.git("commit", "-q", "--allow-empty", "-m", message)
        return self.git("rev-parse", "HEAD")

    def change(self, *paths):
        # Each edit stays valid TOML where the file is TOML, so a manifest or lockfile reaches the rule a test names
        # instead of failing to parse: a crate manifest gains a [features] table, which the manifest rules refuse.
        self.git("checkout", "-q", "--detach", self.base)
        for path in paths:
            if path.endswith("/Cargo.toml") and path != "crates/Cargo.toml":
                edit = "\n[features]\nchanged = []\n"
            elif path.endswith((".toml", ".lock")):
                edit = "# changed\n"
            else:
                edit = "// changed\n"
            self.write(path, (self.TREE.get(path) or "") + edit)
        return self.commit("change " + " ".join(paths))


class TestScopeClassifierTests(ScopeFixture):
    """scripts/ci-test-scope.py on real temporary repositories, against a fake kkernel."""

    def scope(self, base, head, *mode):
        mode = mode or ("--kkernel", str(self.fake / "kkernel"))
        completed = subprocess.run(
            [sys.executable, str(self.repo / "scripts" / "ci-test-scope.py"), base, head, *mode],
            cwd=self.repo, env=self.env, text=True, capture_output=True, timeout=60, check=False,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(completed.stdout.count("\n"), 1, completed.stdout)
        return completed.stdout.strip(), completed.stderr

    def test_fixture_git_turns_off_auto_maintenance(self):
        self.assertEqual(self.git("config", "--get", "maintenance.auto"), "false")

    def test_each_path_alone_under_the_path_rules(self):
        # Every fixture path changed alone: crate source reaches the map, everything else is the full suite.
        for path in sorted(self.TREE):
            with self.subTest(path=path):
                verdict, reasons = self.scope(self.base, self.change(path), "--paths-only")
                admitted = path in self.CRATE_SOURCE and path not in self.NAMED_ELSEWHERE
                self.assertEqual(verdict, "map" if admitted else "full", reasons)
                self.assertNotIn("does not parse", reasons)
        self.assertEqual(self.calls(), [], "--paths-only never runs the binary")

    def test_a_full_suite_path_never_reaches_the_binary(self):
        # ADR-205 D4 ordering arm: a manifest beside crate source prints its reason and no ingest line.
        verdict, reasons = self.scope(self.base, self.change("crates/k/src/lib.rs", "crates/k/Cargo.toml"))
        self.assertEqual(verdict, "full")
        self.assertIn("full suite: manifest: crates/k/Cargo.toml changes features", reasons)
        self.assertNotIn("ingest:", reasons)
        self.assertEqual(self.calls(), [])

    def test_crate_source_prints_the_ingest_line_and_the_closure(self):
        verdict, reasons = self.scope(self.base, self.change("crates/k/src/lib.rs"))
        self.assertEqual(verdict, "j k", reasons)
        self.assertIn("changed: crates/k/src/lib.rs -> k", reasons)
        self.assertIn("ingest: ", reasons)
        self.assertIn("closure (2 of 4 crates): j k", reasons)
        self.assertTrue(self.calls()[0].startswith("code.ingest("), self.calls())

    def test_a_nested_crate_takes_the_longest_manifest_prefix_and_its_transitive_dependents(self):
        verdict, reasons = self.scope(self.base, self.change("crates/k/sub/src/lib.rs"))
        self.assertEqual(verdict, "j k ksub", reasons)
        self.assertIn("changed: crates/k/sub/src/lib.rs -> ksub", reasons)

    def test_a_crate_nothing_depends_on_is_its_own_scope(self):
        self.assertEqual(self.scope(self.base, self.change("crates/m/src/lib.rs"))[0], "m")

    def test_markdown_beside_crate_source(self):
        verdict, reasons = self.scope(self.base, self.change("crates/m/src/lib.rs", "notes/k-notes.md"))
        self.assertEqual(verdict, "m", reasons)
        self.assertIn("documentation only: notes/k-notes.md", reasons)
        # A Markdown file that code names puts the run on the full suite.
        self.write("crates/m/src/lib.rs", 'pub const DOC: &str = include_str!("../../../notes/k-notes.md");\n')
        named = self.commit("name the notes")
        self.write("notes/k-notes.md", "# Notes\n\nMore.\n")
        self.write("crates/m/src/extra.rs", "\n")
        verdict, reasons = self.scope(named, self.commit("notes and source"))
        self.assertEqual(verdict, "full")
        self.assertIn("notes/k-notes.md is named by a non-Markdown file", reasons)

    def test_the_map_is_read_across_pages(self):
        self.set_graph({**self.GRAPH, "pages": [["k"], ["j", "ksub"], ["m"]]})
        verdict, reasons = self.scope(self.base, self.change("crates/k/src/lib.rs"))
        self.assertEqual(verdict, "j k", reasons)
        self.assertEqual(sum(call.startswith("code.list(") for call in self.calls()), 3)

    def test_every_map_defect_runs_the_full_suite(self):
        head = self.change("crates/k/src/lib.rs")
        projects = self.GRAPH["projects"]
        for label, graph, reason in (
                ("refused", {**self.GRAPH, "exit": 1}, "kkernel exec exited 1"),
                ("warning", {**self.GRAPH, "ingest": {"warnings": ["skipped a file"]}}, "reported warnings"),
                ("blocked", {**self.GRAPH, "ingest": {"blocked_count": 1}}, "blocked writes"),
                ("refused manifest", {**self.GRAPH, "ingest": {"manifest_files_refused": 1}},
                 "manifest_files_refused=1"),
                ("truncated", {**self.GRAPH, "truncated": True}, "was truncated"),
                ("not in the map", {**self.GRAPH, "projects": {"j": "p-j", "m": "p-m"}},
                 "changed crate k is not a project"),
                ("unlisted dependent", {**self.GRAPH, "edges": [["p-x", "p-k"]]},
                 "a project that code.list did not"),
                ("duplicate name", {**self.GRAPH, "pages": [["k", "j"], ["k"]]}, "two projects in the map"),
                ("non-project row", {**self.GRAPH, "list_kind": "module"}, "returned a 'module' row"),
                ("empty map", {"projects": {}, "edges": []}, "the map holds no projects")):
            with self.subTest(label=label):
                self.set_graph(graph)
                verdict, reasons = self.scope(self.base, head)
                self.assertEqual(verdict, "full", reasons)
                self.assertIn(reason, reasons)
        # A dependent that is not a project (a module, say) is not a crate to test.
        self.set_graph({**self.GRAPH, "kinds": {"p-j": "module"}})
        self.assertEqual(self.scope(self.base, head)[0], "k")
        self.assertEqual(projects, self.GRAPH["projects"])

    def test_a_file_another_crate_includes_runs_the_full_suite(self):
        # The closure of k (or m) never reaches m (or j): only the include names the file.
        for path, reader in (("crates/k/src/shared.rs", "m (crates/m/tests/reader.rs:1)"),
                             ("crates/m/src/table.txt", "j (crates/j/src/census.rs:1)"),
                             ("crates/m/src/multi.txt", "j (crates/j/src/multi.rs:1)"),
                             ("crates/m/src/raw.txt", "j (crates/j/src/raw.rs:1)"),
                             ("crates/m/src/env.txt", "j (crates/j/tests/env.rs:1)"),
                             ("crates/m/sql/q.sql", "j (crates/j/src/prefix.rs:3)")):
            for mode in (("--paths-only",), ()):
                with self.subTest(path=path, mode=mode):
                    verdict, reasons = self.scope(self.base, self.change(path), *mode)
                    self.assertEqual(verdict, "full", reasons)
                    self.assertIn(f"{path} is compiled or read by another crate: {reader}", reasons)
                    self.assertNotIn("ingest:", reasons)
        self.assertEqual(self.calls(), [])

    def test_the_include_scan_prints_its_population_and_skips_same_crate_includes(self):
        verdict, reasons = self.scope(self.base, self.change("crates/k/src/lib.rs"))
        self.assertEqual(verdict, "j k", reasons)
        self.assertIn("include scan: 7 sites in 8 files, 0 unreadable, 6 naming a path outside the reader's crate", reasons)
        self.assertIn("named from another crate: crates/k/src/shared.rs <- m (crates/m/tests/reader.rs:1)", reasons)
        self.assertIn("named from another crate: crates/m/sql/q* <- j (crates/j/src/prefix.rs:3)", reasons)

    def test_includes_in_comments_and_string_literals_are_not_sites(self):
        verdict, reasons = self.scope(self.base, self.change("crates/m/src/ghost.txt"), "--paths-only")
        self.assertEqual(verdict, "map", reasons)
        self.assertNotIn("ghost.txt <-", reasons)

    def test_an_include_the_scan_cannot_resolve_runs_the_full_suite(self):
        self.write("crates/j/src/bad.rs", "pub const B: &str = include_str!(chosen_path());\n")
        unresolved = self.commit("computed include")
        self.write("crates/m/src/lib.rs", "pub fn h2() {}\n")
        verdict, reasons = self.scope(unresolved, self.commit("change m"), "--paths-only")
        self.assertEqual(verdict, "full", reasons)
        self.assertIn("include scan: cannot resolve 1 include(s) in code: crates/j/src/bad.rs:1", reasons)

    def test_a_path_attribute_inside_an_inline_module_resolves_below_the_module_directory(self):
        # rustc resolves these below the inline module's directory: from the file's directory for a crate root
        # (tests/inline.rs), from a directory named after the file otherwise (src/deep.rs). Both name a file of m.
        self.write("crates/m/src/inl.rs", "pub fn i() {}\n")
        self.write("crates/m/src/deep.rs", "pub fn d() {}\n")
        self.write("crates/j/tests/inline.rs", "mod inner {\n    #[path = \"../../../m/src/inl.rs\"]\n    mod x;\n}\n")
        self.write("crates/j/src/deep.rs", "mod inner {\n    #[path = \"../../../../m/src/deep.rs\"]\n    mod x;\n}\n")
        # an inline module that carries its own path attribute contributes that path as its directory: only the
        # two-level "a/b" reaches m, the one-level module name does not
        self.write("crates/m/src/esc.rs", "pub fn e() {}\n")
        self.write("crates/j/tests/over.rs",
                   "#[path = \"a/b\"]\nmod thread {\n    #[path = \"../../../../m/src/esc.rs\"]\n    mod local;\n}\n")
        base = self.commit("inline module readers")
        for path, reader in (("crates/m/src/inl.rs", "j (crates/j/tests/inline.rs:2)"),
                             ("crates/m/src/deep.rs", "j (crates/j/src/deep.rs:2)"),
                             ("crates/m/src/esc.rs", "j (crates/j/tests/over.rs:3)")):
            with self.subTest(path=path):
                self.git("checkout", "-q", "--detach", base)
                self.write(path, "pub fn changed() {}\n")
                verdict, reasons = self.scope(base, self.commit("change " + path), "--paths-only")
                self.assertEqual(verdict, "full", reasons)
                self.assertIn(f"{path} is compiled or read by another crate: {reader}", reasons)

    def test_a_path_attribute_nested_in_cfg_attr_at_any_depth_is_a_site(self):
        self.write("crates/m/src/nested.rs", "pub fn n() {}\n")
        self.write("crates/j/src/nested.rs",
                   "#[cfg_attr(all(), cfg_attr(not(any()), path = \"../../m/src/nested.rs\"))]\nmod y;\n")
        base = self.commit("nested cfg_attr reader")
        self.write("crates/m/src/nested.rs", "pub fn changed() {}\n")
        verdict, reasons = self.scope(base, self.commit("change nested"), "--paths-only")
        self.assertEqual(verdict, "full", reasons)
        self.assertIn("crates/m/src/nested.rs is compiled or read by another crate: j (crates/j/src/nested.rs:1)", reasons)

    def test_an_include_path_with_a_line_break_runs_the_full_suite_and_is_never_printed(self):
        # a raw string can hold a newline; printed as is, the text after it would start a new job-log line
        self.write("crates/j/src/broken.rs", "pub const B: &str = include_str!(r\"../../m/src/x\n::error::injected\");\n")
        base = self.commit("include with a line break")
        self.write("crates/m/src/lib.rs", "pub fn h2() {}\n")
        verdict, reasons = self.scope(base, self.commit("change m"), "--paths-only")
        self.assertEqual(verdict, "full", reasons)
        self.assertIn("crates/j/src/broken.rs:1 (a path with a line break)", reasons)
        self.assertFalse([line for line in reasons.splitlines() if line.startswith("::")], reasons)

    def test_an_include_scan_that_finds_nothing_runs_the_full_suite(self):
        for path in ("crates/m/tests/reader.rs", "crates/j/src/census.rs", "crates/k/tests/own.rs",
                     "crates/j/src/multi.rs", "crates/j/src/raw.rs", "crates/j/tests/env.rs", "crates/j/src/prefix.rs"):
            (self.repo / path).unlink()
        stripped = self.commit("no include sites")
        self.write("crates/m/src/lib.rs", "pub fn h2() {}\n")
        verdict, reasons = self.scope(stripped, self.commit("change m"), "--paths-only")
        self.assertEqual(verdict, "full", reasons)
        self.assertIn("include scan: BROKEN", reasons)

    def test_a_missing_binary_runs_the_full_suite(self):
        head = self.change("crates/k/src/lib.rs")
        verdict, reasons = self.scope(self.base, head, "--kkernel", str(self.root / "absent"))
        self.assertEqual(verdict, "full")
        self.assertIn("no executable kkernel binary", reasons)

    def test_no_change_an_unknown_revision_and_a_submodule_run_the_full_suite(self):
        verdict, reasons = self.scope(self.base, self.commit("empty"))
        self.assertEqual(verdict, "full")
        self.assertIn("no changed paths", reasons)
        verdict, reasons = self.scope("0" * 40, self.base)
        self.assertEqual(verdict, "full")
        self.assertIn("git diff", reasons)
        self.write("crates/k/src/lib.rs", "pub fn g() {}\n")
        self.git("add", "-A")
        self.git("update-index", "--add", "--cacheinfo", f"160000,{self.base},crates/vendored")
        self.git("commit", "-q", "-m", "submodule")
        verdict, reasons = self.scope(self.base, self.git("rev-parse", "HEAD"))
        self.assertEqual(verdict, "full")
        self.assertIn("has a submodule", reasons)
        self.assertEqual(self.calls(), [])

    def test_a_move_out_of_crates_is_classified_by_its_new_name(self):
        self.git("checkout", "-q", "--detach", self.base)
        self.git("mv", "crates/k/src/lib.rs", "scripts/lib.rs")
        verdict, reasons = self.scope(self.base, self.commit("moved"), "--paths-only")
        self.assertEqual(verdict, "full")
        self.assertIn("CI or lint script: scripts/lib.rs", reasons)

    def test_usage(self):
        for args in ([], ["a", "b"], ["a", "b", "--kkernel"], ["a", "b", "--paths-only", "x"], ["a", "b", "--other"]):
            with self.subTest(args=args):
                completed = subprocess.run(
                    [sys.executable, str(self.repo / "scripts" / "ci-test-scope.py"), *args],
                    cwd=self.repo, env=self.env, text=True, capture_output=True, timeout=30, check=False,
                )
                self.assertEqual(completed.returncode, 2, completed.stderr)
                self.assertIn("usage:", completed.stderr)


REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"


def lockfile(*packages, version=4):
    """A Cargo.lock: each package is (name, version, source or None, checksum or None, [dependency strings])."""
    blocks = [f"version = {version}\n"]
    for name, number, source, checksum, dependencies in packages:
        lines = [f'[[package]]\nname = "{name}"\nversion = "{number}"\n']
        if source:
            lines.append(f'source = "{source}"\n')
        if checksum:
            lines.append(f'checksum = "{checksum}"\n')
        if dependencies:
            lines.append("dependencies = [\n" + "".join(f' "{d}",\n' for d in dependencies) + "]\n")
        blocks.append("".join(lines))
    return "\n".join(blocks)


class ManifestAndLockfileScopeTests(ScopeFixture):
    """ADR-205 Amendment 1, D2 and D3: a crate's own manifest and crates/Cargo.lock are read as data."""

    # j depends on k and rand 0.8, k on ksub and serde (inherited from the workspace with a feature), m has a
    # development dependency on rand 0.7: two versions of one package, each reached by a different crate.
    PACKAGES = (("j", "0.1.0", None, None, ["k", "rand 0.8.0"]),
                ("k", "0.1.0", None, None, ["ksub", "serde"]),
                ("ksub", "0.1.0", None, None, []),
                ("m", "0.1.0", None, None, ["rand 0.7.0"]),
                ("rand", "0.7.0", REGISTRY, "a7", []),
                ("rand", "0.8.0", REGISTRY, "a8", []),
                ("serde", "1.0.0", REGISTRY, "c1", []))
    K = ('[package]\nname = "k"\nversion = "0.1.0"\n\n[dependencies]\nksub = { path = "sub" }\n'
         'serde = { workspace = true }\n\n[[test]]\nname = "t"\npath = "tests/t.rs"\n')
    J = '[package]\nname = "j"\nversion = "0.1.0"\n\n[dependencies]\nk = { path = "../k" }\nrand = "0.8"\n'
    M = '[package]\nname = "m"\nversion = "0.1.0"\n\n[dev-dependencies]\nrand = "0.7"\n'
    TREE = {**ScopeFixture.TREE,
            "crates/Cargo.toml": ('[workspace]\nmembers = ["k", "k/sub", "j", "m"]\n\n[workspace.dependencies]\n'
                                  'serde = { version = "1", features = ["derive"] }\n'),
            "crates/Cargo.lock": lockfile(*PACKAGES),
            "crates/k/Cargo.toml": K,
            "crates/k/sub/Cargo.toml": '[package]\nname = "ksub"\nversion = "0.1.0"\n',
            "crates/j/Cargo.toml": J,
            "crates/m/Cargo.toml": M}

    scope = TestScopeClassifierTests.scope

    def edit(self, files, base=None, links=()):
        """A commit on base (the fixture base by default) writing files and adding (link, target) symlinks."""
        self.git("checkout", "-q", "--detach", base or self.base)
        for path, text in files.items():
            if (self.repo / path).is_symlink():
                (self.repo / path).unlink()
            self.write(path, text)
        for path, target in links:
            (self.repo / path).parent.mkdir(parents=True, exist_ok=True)
            (self.repo / path).unlink(missing_ok=True)
            os.symlink(target, self.repo / path)
        return self.commit("edit " + " ".join(sorted(files)))

    def ingested(self):
        return json.loads((self.fake / "ingested.json").read_text())

    def lock_with(self, **changes):
        """The fixture lockfile with packages replaced by name@version: {"rand@0.8.0": (name, version, ...)}."""
        packages = [changes.pop(f"{p[0]}@{p[1]}", p) for p in self.PACKAGES]
        return lockfile(*[p for p in packages if p is not None], *changes.values())

    # --- crate manifests (verify-by 2 and 5) ---

    def test_a_new_development_dependency_runs_the_crate_and_every_member_reaching_the_package(self):
        head = self.edit({"crates/m/Cargo.toml": self.M + 'serde = "1"\n',
                          "crates/Cargo.lock": self.lock_with(**{"m@0.1.0": ("m", "0.1.0", None, None,
                                                                             ["rand 0.7.0", "serde"])})})
        verdict, reasons = self.scope(self.base, head)
        self.assertEqual(verdict, "j k m", reasons)
        self.assertIn("changed: crates/m/Cargo.toml -> m (manifest; changed dependency entries name: serde)", reasons)
        self.assertIn("members reaching serde in either lockfile: j k m", reasons)
        self.assertIn("ingest: the base tree", reasons)
        # the binary was handed the base commit's manifests, never one the change wrote
        seen = self.ingested()
        self.assertNotEqual(seen["path"], str(self.repo / "crates"))
        self.assertEqual(seen["manifests"]["m/Cargo.toml"], self.M)
        self.assertEqual(seen["manifests"]["k/Cargo.toml"], self.K)
        self.assertEqual(self.scope(self.base, head, "--paths-only")[0], "map")

    def test_a_dependency_named_by_neither_lockfile_runs_the_full_suite(self):
        verdict, reasons = self.scope(self.base, self.edit({"crates/m/Cargo.toml": self.M + 'itoa = "1"\n'}))
        self.assertEqual(verdict, "full", reasons)
        self.assertIn("lockfile: neither side lists itoa", reasons)

    def test_features_runs_the_full_suite(self):
        for label, path, text, reason in (
                ("[features]", "crates/m/Cargo.toml", self.M + "\n[features]\nx = []\n", "changes features"),
                ("dependency features", "crates/j/Cargo.toml",
                 self.J.replace('rand = "0.8"', 'rand = { version = "0.8", features = ["small_rng"] }'),
                 "changes features, default-features or optional of dependencies.rand"),
                ("default-features", "crates/j/Cargo.toml",
                 self.J.replace('rand = "0.8"', 'rand = { version = "0.8", default-features = false }'),
                 "of dependencies.rand"),
                ("optional", "crates/j/Cargo.toml",
                 self.J.replace('rand = "0.8"', 'rand = { version = "0.8", optional = true }'),
                 "of dependencies.rand"),
                ("inherited entry gains a local feature", "crates/k/Cargo.toml",
                 self.K.replace("serde = { workspace = true }", 'serde = { workspace = true, features = ["std"] }'),
                 "of dependencies.serde"),
                # a new entry inheriting the workspace's feature list gains features it did not have
                ("new inherited entry", "crates/m/Cargo.toml", self.M + "serde = { workspace = true }\n",
                 "of dev-dependencies.serde"),
                ("target-specific features", "crates/m/Cargo.toml",
                 self.M + '\n[target.\'cfg(unix)\'.dependencies]\nrand = { version = "0.7", features = ["x"] }\n',
                 "of target.cfg(unix).dependencies.rand")):
            with self.subTest(label=label):
                verdict, reasons = self.scope(self.base, self.edit({path: text}))
                self.assertEqual(verdict, "full", reasons)
                self.assertIn(f"full suite: manifest: {path} ", reasons)
                self.assertIn(reason, reasons)
                self.assertNotIn("ingest:", reasons)
                self.assertEqual(self.calls(), [])

    def test_a_target_outside_the_crate_or_with_changed_required_features_runs_the_full_suite(self):
        test_table = '[[test]]\nname = "t"\npath = "tests/t.rs"\n'
        for label, text, reason in (
                ("outside path", self.K.replace(test_table, '[[test]]\nname = "t"\npath = "../j/src/lib.rs"\n'),
                 "gives [[test]] t the path ../j/src/lib.rs, which resolves to crates/j/src/lib.rs, outside the crate"),
                ("nested crate's file", self.K + '\n[[test]]\nname = "s"\npath = "sub/src/lib.rs"\n',
                 "gives [[test]] s the path sub/src/lib.rs, which resolves to crates/k/sub/src/lib.rs"),
                ("required features", self.K.replace(test_table, test_table + 'required-features = ["x"]\n'),
                 "changes the required features of [[test]] t"),
                ("absolute path", self.K + '\n[[bench]]\nname = "b"\npath = "/etc/b.rs"\n',
                 "gives [[bench]] b a path that is not relative")):
            with self.subTest(label=label):
                verdict, reasons = self.scope(self.base, self.edit({"crates/k/Cargo.toml": text}))
                self.assertEqual(verdict, "full", reasons)
                self.assertIn(reason, reasons)

    def test_a_target_path_through_a_symbolic_link_out_of_the_crate_runs_the_full_suite(self):
        linked = self.edit({}, links=[("crates/k/tests/link.rs", "../../j/src/lib.rs"),
                                      ("crates/k/inside.rs", "src/lib.rs")])
        outside = self.edit({"crates/k/Cargo.toml": self.K + '\n[[test]]\nname = "l"\npath = "tests/link.rs"\n'},
                            base=linked)
        verdict, reasons = self.scope(linked, outside)
        self.assertEqual(verdict, "full", reasons)
        self.assertIn("gives [[test]] l the path tests/link.rs, which resolves to crates/j/src/lib.rs", reasons)
        # control: a link that stays inside the crate is admitted
        inside = self.edit({"crates/k/Cargo.toml": self.K + '\n[[test]]\nname = "i"\npath = "inside.rs"\n'},
                           base=linked)
        self.assertEqual(self.scope(linked, inside)[0], "j k")

    def test_a_new_bench_inside_the_crate_runs_the_crate_closure(self):
        head = self.edit({"crates/k/Cargo.toml": self.K + '\n[[bench]]\nname = "kb"\npath = "benches/kb.rs"\n',
                          "crates/k/benches/kb.rs": "fn main() {}\n"})
        verdict, reasons = self.scope(self.base, head)
        self.assertEqual(verdict, "j k", reasons)
        self.assertIn("changed: crates/k/Cargo.toml -> k (manifest; changed dependency entries name: none)", reasons)
        self.assertEqual(self.ingested()["manifests"]["k/Cargo.toml"], self.K)

    def test_any_other_manifest_difference_runs_the_full_suite(self):
        for label, path, text, reason in (
                ("[package]", "crates/m/Cargo.toml", self.M.replace('version = "0.1.0"', 'version = "0.2.0"'),
                 "manifest: crates/m/Cargo.toml changes package.version, outside the dependency and target tables"),
                ("[package.metadata]", "crates/m/Cargo.toml", self.M + "\n[package.metadata.docs]\nall = true\n",
                 "changes package.metadata"),
                ("[lints]", "crates/m/Cargo.toml", self.M + "\n[lints.rust]\nunsafe_code = \"forbid\"\n",
                 "changes lints"),
                ("workspace manifest", "crates/Cargo.toml", self.TREE["crates/Cargo.toml"] + "resolver = \"2\"\n",
                 "manifest: crates/Cargo.toml"),
                ("a manifest that is not a crate's", "crates/k/tests/fixture/Cargo.toml", "[workspace]\n",
                 "manifest: crates/k/tests/fixture/Cargo.toml is not the manifest of a crate at"),
                ("a new crate", "crates/n/Cargo.toml", '[package]\nname = "n"\n',
                 "manifest: crates/n/Cargo.toml cannot be read at")):
            with self.subTest(label=label):
                verdict, reasons = self.scope(self.base, self.edit({path: text}))
                self.assertEqual(verdict, "full", reasons)
                self.assertIn(reason, reasons)
                self.assertEqual(self.calls(), [])

    def test_a_manifest_another_crate_reads_runs_the_full_suite(self):
        reader = self.edit({"crates/j/tests/manifest.rs": 'const M: &str = include_str!("../../m/Cargo.toml");\n'})
        head = self.edit({"crates/m/Cargo.toml": self.M + '\n[[example]]\nname = "e"\n'}, base=reader)
        verdict, reasons = self.scope(reader, head)
        self.assertEqual(verdict, "full", reasons)
        self.assertIn("crates/m/Cargo.toml is compiled or read by another crate: j (crates/j/tests/manifest.rs:1)",
                      reasons)

    # --- the lockfile (verify-by 3) ---

    def test_a_lockfile_change_runs_the_members_that_reach_it(self):
        for label, changes, expected in (
                ("bump one external package", {"rand@0.7.0": ("rand", "0.7.1", REGISTRY, "a71", []),
                                               "m@0.1.0": ("m", "0.1.0", None, None, ["rand 0.7.1"])}, "m"),
                ("checksum alone", {"serde@1.0.0": ("serde", "1.0.0", REGISTRY, "c2", [])}, "j k"),
                ("dependency list alone", {"rand@0.8.0": ("rand", "0.8.0", REGISTRY, "a8", ["serde"])}, "j"),
                # two versions of rand: only the changed one's reachers run
                ("one of two versions", {"rand@0.7.0": ("rand", "0.7.0", REGISTRY, "a7x", [])}, "m")):
            with self.subTest(label=label):
                head = self.edit({"crates/Cargo.lock": self.lock_with(**changes)})
                verdict, reasons = self.scope(self.base, head)
                self.assertEqual(verdict, expected, reasons)
                self.assertIn("lockfile: ", reasons)
                self.assertNotIn("ingest: the base tree", reasons)

    def test_a_lockfile_header_change_a_parse_failure_or_a_change_reaching_every_member_runs_the_full_suite(self):
        everyone = self.lock_with(**{"ksub@0.1.0": ("ksub", "0.1.0", None, None, ["serde"]),
                                     "rand@0.7.0": ("rand", "0.7.0", REGISTRY, "a7x", [])})
        for label, text, reason in (
                ("header", lockfile(*self.PACKAGES, version=3), "lockfile: crates/Cargo.lock changes its version header"),
                ("unparseable", self.TREE["crates/Cargo.lock"] + "// changed\n",
                 "lockfile: crates/Cargo.lock does not parse at"),
                ("ambiguous dependency", self.lock_with(**{"m@0.1.0": ("m", "0.1.0", None, None, ["rand"])}),
                 "depends on 'rand', which matches 2 entries"),
                ("every member", everyone, "lockfile: the change reaches every workspace member (4)")):
            with self.subTest(label=label):
                verdict, reasons = self.scope(self.base, self.edit({"crates/Cargo.lock": text}))
                self.assertEqual(verdict, "full", reasons)
                self.assertIn(reason, reasons)
                self.assertEqual(self.calls(), [])

    def test_a_lockfile_a_crate_reads_runs_the_full_suite(self):
        reader = self.edit({"crates/j/tests/lock.rs": 'const L: &str = include_str!("../../Cargo.lock");\n'})
        head = self.edit({"crates/Cargo.lock": self.lock_with(**{"serde@1.0.0": ("serde", "1.0.0", REGISTRY, "c2",
                                                                                 [])})}, base=reader)
        verdict, reasons = self.scope(reader, head)
        self.assertEqual(verdict, "full", reasons)
        self.assertIn("crates/Cargo.lock is read by a crate: j (crates/j/tests/lock.rs:1)", reasons)

    # --- symbolic links: git stores a link's target text, cargo reads the file the link names ---
    # Each link's target text is itself an admissible change, so only the file mode separates these from "map".

    def test_a_manifest_that_becomes_a_symbolic_link_runs_the_full_suite(self):
        lock = self.lock_with(**{"m@0.1.0": ("m", "0.1.0", None, None, ["rand 0.7.0", "serde"])})
        head = self.edit({"crates/Cargo.lock": lock}, links=[("crates/m/Cargo.toml", self.M + 'serde = "1"\n')])
        self.assertEqual(self.git("ls-tree", head, "crates/m/Cargo.toml").split()[0], "120000")
        verdict, reasons = self.scope(self.base, head)
        self.assertEqual(verdict, "full", reasons)
        self.assertIn(f"full suite: crates/m/Cargo.toml is a symbolic link at {head}", reasons)
        self.assertEqual(self.calls(), [])
        self.assertEqual(self.scope(self.base, head, "--paths-only")[0], "full")

    def test_a_manifest_that_was_a_symbolic_link_runs_the_full_suite(self):
        linked = self.edit({}, links=[("crates/m/Cargo.toml", self.M)])
        lock = self.lock_with(**{"m@0.1.0": ("m", "0.1.0", None, None, ["rand 0.7.0", "serde"])})
        head = self.edit({"crates/m/Cargo.toml": self.M + 'serde = "1"\n', "crates/Cargo.lock": lock}, base=linked)
        self.assertEqual(self.git("ls-tree", head, "crates/m/Cargo.toml").split()[0], "100644")
        verdict, reasons = self.scope(linked, head, "--paths-only")
        self.assertEqual(verdict, "full", reasons)
        self.assertIn(f"full suite: crates/m/Cargo.toml is a symbolic link at {linked}", reasons)

    def test_a_lockfile_that_becomes_a_symbolic_link_runs_the_full_suite(self):
        lock = self.lock_with(**{"rand@0.7.0": ("rand", "0.7.0", REGISTRY, "a7x", [])})
        head = self.edit({}, links=[("crates/Cargo.lock", lock)])
        self.assertEqual(self.git("ls-tree", head, "crates/Cargo.lock").split()[0], "120000")
        verdict, reasons = self.scope(self.base, head)
        self.assertEqual(verdict, "full", reasons)
        self.assertIn(f"full suite: crates/Cargo.lock is a symbolic link at {head}", reasons)
        self.assertEqual(self.calls(), [])
        self.assertEqual(self.scope(self.base, head, "--paths-only")[0], "full")


class ScopeStepTests(ScopeFixture):
    """The Classify job's Scope step: the path rules first, then the binary from main, never a failure."""

    FAKE_GH = """#!/bin/bash
echo "$*" >> "$GH_LOG"
case "$1 $2" in
  "api "*) [ -n "$GH_RUNS" ] || exit 1; printf '%s\\n' $GH_RUNS ;;
  "run download")
    dir="${@: -1}"
    for run in $GH_ARTIFACT_RUNS; do
      if [ "$run" = "$3" ]; then mkdir -p "$dir"; cp "$FAKE_DIR/kkernel" "$FAKE_DIR/graph.json" "$dir/"; exit 0; fi
    done
    exit 1 ;;
  *) exit 1 ;;
esac
"""

    def setUp(self):
        super().setUp()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        tools = {"gh": self.FAKE_GH, "python3": f'#!/bin/bash\nexec "{sys.executable}" "$@"\n'}
        if shutil.which("timeout") is None:
            tools["timeout"] = '#!/bin/bash\nshift\nexec "$@"\n'
        for name, text in tools.items():
            (self.bin / name).write_text(text)
            (self.bin / name).chmod(0o755)
        self.runner_temp = self.root / "runner-temp"
        self.runner_temp.mkdir()
        self.gh_log = self.root / "gh.log"

    def run_step(self, event_name, head, runs="201 202", artifact_runs="202", extra_env=None):
        step = indented_block(workflow_text("ci.yml"), "changed-paths", 2).split("- name: Scope\n", 1)[1]
        match = re.search(r"(?m)^        run: \|\n((?:          .*\n|\n)+)", step + "\n")
        self.assertIsNotNone(match)
        output = self.root / "output"
        output.write_text("")
        completed = subprocess.run(
            ["bash", "-c", textwrap.dedent(match.group(1))], cwd=self.repo, text=True, capture_output=True,
            timeout=60, check=False,
            env={**self.env, "PATH": f"{self.bin}:{os.environ['PATH']}", "GITHUB_OUTPUT": str(output),
                 "RUNNER_TEMP": str(self.runner_temp), "EVENT_NAME": event_name, "HEAD_SHA": head,
                 "REPOSITORY": "o/r", "GH_TOKEN": "x", "GH_LOG": str(self.gh_log), "GH_RUNS": runs,
                 "GH_ARTIFACT_RUNS": artifact_runs, "FAKE_DIR": str(self.fake), **(extra_env or {})},
        )
        self.assertEqual(completed.returncode, 0, completed.stdout + completed.stderr)
        lines = output.read_text().splitlines()
        self.assertEqual(len(lines), 1, lines)
        self.assertTrue(lines[0].startswith("test_scope="), lines)
        return lines[0].split("=", 1)[1], completed.stdout + completed.stderr

    def gh_calls(self):
        return self.gh_log.read_text().splitlines() if self.gh_log.exists() else []

    def test_a_crate_source_pull_request_is_scoped_with_the_binary_from_main(self):
        scope, log = self.run_step("pull_request", self.change("crates/k/src/lib.rs"))
        self.assertEqual(scope, "j k", log)
        self.assertIn("kkernel from main push run 202", log)
        downloads = [call for call in self.gh_calls() if call.startswith("run download")]
        self.assertEqual([call.split()[2] for call in downloads], ["201", "202"])
        self.assertTrue(self.calls(self.runner_temp / "kkernel-main-202"))

    def test_the_path_rules_run_before_any_fetch(self):
        scope, log = self.run_step("pull_request", self.change("crates/k/src/lib.rs", "crates/k/Cargo.toml"))
        self.assertEqual(scope, "full", log)
        self.assertIn("full suite: manifest: crates/k/Cargo.toml changes features", log)
        self.assertEqual(self.gh_calls(), [])

    def test_other_events_are_never_scoped(self):
        head = self.change("crates/k/src/lib.rs")
        for event_name in ("push", "schedule", "workflow_dispatch"):
            with self.subTest(event_name=event_name):
                scope, log = self.run_step(event_name, head)
                self.assertEqual(scope, "full", log)
        self.assertEqual(self.gh_calls(), [])

    def train(self):
        """Two commits after the tested base: k's source, then m's. The pushed head's parent holds only k's change."""
        self.change("crates/k/src/lib.rs")
        self.write("crates/m/src/lib.rs", self.TREE["crates/m/src/lib.rs"] + "// changed\n")
        return self.commit("change crates/m/src/lib.rs")

    def test_an_integration_push_is_scoped_from_the_tested_main_base(self):
        head = self.train()
        scope, log = self.run_step("push", head, extra_env={"REF": "refs/heads/integration/train-1",
                                                            "TESTED_BASE": self.base})
        self.assertEqual(scope, "j k m", log)
        self.assertIn("kkernel from main push run 202", log)

    def test_an_integration_push_without_a_tested_base_runs_the_full_suite(self):
        head = self.train()
        for tested_base in ("", None):
            with self.subTest(tested_base=tested_base):
                extra = {"REF": "refs/heads/integration/train-1"}
                if tested_base is not None:
                    extra["TESTED_BASE"] = tested_base
                scope, log = self.run_step("push", head, extra_env=extra)
                self.assertEqual(scope, "full", log)
                self.assertIn("no fully tested main commit is an ancestor of this push", log)
        self.assertEqual(self.gh_calls(), [])

    # A scope script that scopes every change to crate k and leaves a mark when it runs.
    LENIENT_SCOPE = ("import pathlib, sys\n"
                     "pathlib.Path(__file__).with_name('lenient-ran').write_text(' '.join(sys.argv[1:]))\n"
                     "print('map' if '--paths-only' in sys.argv else 'k')\n")

    def lenient_scope_change(self):
        """Crate k's source and m's, with a scope script that would scope both to k."""
        self.change("crates/k/src/lib.rs")
        self.write("crates/m/src/lib.rs", self.TREE["crates/m/src/lib.rs"] + "// changed\n")
        self.write("scripts/ci-test-scope.py", self.LENIENT_SCOPE)
        return self.commit("a lenient scope script beside crate source")

    def test_a_change_that_edits_a_classifier_runs_the_full_suite_without_running_it(self):
        head = self.lenient_scope_change()
        for event_name, extra in (("pull_request", None),
                                  ("push", {"REF": "refs/heads/integration/train-1", "TESTED_BASE": self.base})):
            with self.subTest(event_name=event_name):
                scope, log = self.run_step(event_name, head, extra_env=extra)
                self.assertEqual(scope, "full", log)
                self.assertIn("full suite: the change edits a path classifier: scripts/ci-test-scope.py\n", log)
                self.assertFalse((self.repo / "scripts" / "lenient-ran").exists(), log)
        self.assertEqual(self.gh_calls(), [])

    def test_a_range_that_cannot_be_compared_counts_as_a_classifier_change(self):
        head = self.train()
        missing = "0" * 40
        scope, log = self.run_step("push", head, extra_env={"REF": "refs/heads/integration/train-1",
                                                            "TESTED_BASE": missing})
        self.assertEqual(scope, "full", log)
        self.assertIn(f"full suite: the path classifiers could not be compared with {missing}\n", log)
        self.assertEqual(self.gh_calls(), [])

    def test_a_main_push_is_never_scoped_even_with_a_tested_base(self):
        head = self.train()
        scope, log = self.run_step("push", head, extra_env={"REF": "refs/heads/main", "TESTED_BASE": self.base})
        self.assertEqual(scope, "full", log)
        self.assertIn("full suite: push runs on refs/heads/main are never scoped", log)
        self.assertEqual(self.gh_calls(), [])

    def test_an_unavailable_binary_runs_the_full_suite(self):
        head = self.change("crates/k/src/lib.rs")
        for runs, artifact_runs in (("", "202"), ("201 202", ""), ("201", "202")):
            with self.subTest(runs=runs, artifact_runs=artifact_runs):
                scope, log = self.run_step("pull_request", head, runs=runs, artifact_runs=artifact_runs)
                self.assertEqual(scope, "full", log)
                self.assertIn("no successful main push run", log)

    def test_a_failing_classifier_runs_the_full_suite(self):
        self.set_graph({**self.GRAPH, "exit": 3})
        scope, log = self.run_step("pull_request", self.change("crates/k/src/lib.rs"))
        self.assertEqual(scope, "full", log)
        (self.repo / "scripts" / "ci-test-scope.py").write_text("import sys\nsys.exit(1)\n")
        scope, log = self.run_step("pull_request", self.commit("broken classifier"))
        self.assertEqual(scope, "full", log)


class BenchTrainScopeTests(ScopeFixture):
    """bench-track.yml on a merge train's push: the train's closure, mapped to the components."""

    COMPONENTS = ("scoring_retrieval", "ann_query", "storage_brain", "packs", "runtime_mcp", "gate_channel_code")

    def setUp(self):
        super().setUp()
        self.bin = self.root / "bin"
        self.bin.mkdir()
        tools = {"gh": ScopeStepTests.FAKE_GH, "python3": f'#!/bin/bash\nexec "{sys.executable}" "$@"\n'}
        if shutil.which("timeout") is None:
            tools["timeout"] = '#!/bin/bash\nshift\nexec "$@"\n'
        for name, text in tools.items():
            (self.bin / name).write_text(text)
            (self.bin / name).chmod(0o755)
        self.runner_temp = self.root / "runner-temp"
        self.runner_temp.mkdir()
        self.gh_log = self.root / "gh.log"

    def step(self, name):
        job = indented_block(indented_block(workflow_text("bench-track.yml"), "jobs", 0), "changes", 2)
        step = job.split(f"- name: {name}\n", 1)[1]
        match = re.search(r"(?m)^        run: \|\n((?:          .*\n|\n)+)", step + "\n")
        self.assertIsNotNone(match)
        return textwrap.dedent(match.group(1))

    def run_script(self, script, env, cwd):
        output = self.root / "output"
        output.write_text("")
        completed = subprocess.run(
            ["bash", "-c", script], cwd=cwd, text=True, capture_output=True, timeout=60, check=False,
            env={**self.env, "PATH": f"{self.bin}:{os.environ['PATH']}", "GITHUB_OUTPUT": str(output), **env},
        )
        self.assertEqual(completed.returncode, 0, completed.stdout + completed.stderr)
        values = dict(line.split("=", 1) for line in output.read_text().splitlines())
        return values, completed.stdout + completed.stderr

    def scope(self, head, main_ref="main", runs="201 202", artifact_runs="202"):
        values, log = self.run_script(self.step("Scope the train"), {
            "RUNNER_TEMP": str(self.runner_temp), "HEAD_SHA": head, "MAIN_REF": main_ref, "REPOSITORY": "o/r",
            "GH_TOKEN": "x", "GH_LOG": str(self.gh_log), "GH_RUNS": runs, "GH_ARTIFACT_RUNS": artifact_runs,
            "FAKE_DIR": str(self.fake)}, self.repo)
        self.assertEqual(list(values), ["test_scope"], log)
        return values["test_scope"], log

    def components(self, train_scope):
        values, log = self.run_script(self.step("Map changed paths to components"), {
            "EVENT_NAME": "push", "BASE_SHA": "", "HEAD_SHA": "", "TRAIN_SCOPE": train_scope}, self.root)
        self.assertEqual(values.pop("selective"), "true", log)
        return {name for name in self.COMPONENTS if values[name] == "true"}

    def gh_calls(self):
        return self.gh_log.read_text().splitlines() if self.gh_log.exists() else []

    def test_a_train_is_scoped_over_its_range_from_the_merge_base_with_main(self):
        scope, log = self.scope(self.change("crates/k/src/lib.rs"))
        self.assertEqual(scope, "j k", log)
        self.assertIn(f"train range: {self.base}..", log)
        self.assertIn("kkernel from main push run 202", log)

    def test_a_manifest_in_the_train_selects_every_component_before_any_fetch(self):
        scope, log = self.scope(self.change("crates/k/src/lib.rs", "crates/k/Cargo.toml"))
        self.assertEqual(scope, "full", log)
        self.assertIn("manifest: crates/k/Cargo.toml changes features", log)
        self.assertEqual(self.gh_calls(), [])
        self.assertEqual(self.components(scope), set(self.COMPONENTS))

    def test_every_failure_selects_every_component(self):
        head = self.change("crates/k/src/lib.rs")
        for main_ref, runs, artifact_runs in (("no-such-ref", "201 202", "202"), ("main", "", "202"),
                                              ("main", "201", "202")):
            with self.subTest(main_ref=main_ref, runs=runs):
                scope, log = self.scope(head, main_ref=main_ref, runs=runs, artifact_runs=artifact_runs)
                self.assertEqual(scope, "full", log)
        self.assertEqual(self.components(""), set(self.COMPONENTS))

    def test_the_closure_maps_to_the_components_that_hold_its_crates(self):
        self.assertEqual(self.components("khive-score khive-db khive-runtime"),
                         {"scoring_retrieval", "storage_brain", "runtime_mcp"})
        self.assertEqual(self.components("khive-pack-kg"), {"packs"})
        self.assertEqual(self.components("khive-runtime kkernel"), {"runtime_mcp"})
        self.assertEqual(self.components("khive-channel khive-pack-code"), {"gate_channel_code"})
        self.assertEqual(self.components("khive-changeset khive-wire-protocol"), {"gate_channel_code"})
        self.assertEqual(self.components("khive-types khive-storage"), set())

    def test_the_binary_lookup_is_the_ci_scope_steps_own(self):
        def shared(script):
            lines = script.splitlines()
            start = next(i for i, line in enumerate(lines) if line.lstrip().startswith("mode=$("))
            end = next(i for i in range(start, len(lines)) if lines[i] == "fi")
            return "\n".join(lines[start:end])

        ci = indented_block(workflow_text("ci.yml"), "changed-paths", 2).split("- name: Scope\n", 1)[1]
        ci_run = textwrap.dedent(re.search(r"(?m)^        run: \|\n((?:          .*\n|\n)+)", ci + "\n").group(1))
        self.assertEqual(shared(self.step("Scope the train")), shared(ci_run))

    def test_every_benchmarked_crate_is_in_exactly_one_component(self):
        # A component may name a crate whose first benchmark has not landed yet (the component job leaves it out),
        # so the components cover the benchmarked crates rather than equal them. Every name is a workspace package.
        import tomllib

        text = workflow_text("bench-track.yml")
        inputs = dict(re.findall(r"(?m)^      component: (\S+)\n      crates: (.+)$", text))
        inputs = {name.replace("-", "_"): sorted(crates.split()) for name, crates in inputs.items()}
        self.assertEqual(sorted(inputs), sorted(self.COMPONENTS))
        table = re.findall(r"(?m)^                ((?:crates/[a-z0-9-]+/\*\|?)+)\)\n                  (\w+)=true$",
                           text)
        table = {flag: sorted(path.split("/")[1] for path in paths.split("|")) for paths, flag in table}
        self.assertEqual(table, inputs, "the path table and the component jobs name different crates")
        benchmarked, packages = set(), {}
        for manifest in sorted((REPO_ROOT / "crates").glob("*/Cargo.toml")):
            data = tomllib.loads(manifest.read_text())
            if "package" not in data:
                continue
            packages[manifest.parent.name] = data["package"]["name"]
            auto = data["package"].get("autobenches", True) and list((manifest.parent / "benches").glob("*.rs"))
            if data.get("bench") or auto:
                benchmarked.add(manifest.parent.name)
                self.assertEqual(data["package"]["name"], manifest.parent.name, manifest)
        members = [crate for crates in inputs.values() for crate in crates]
        self.assertEqual(len(members), len(set(members)), "a crate is in two components")
        self.assertTrue(benchmarked, "no benchmarked crate found: the manifest scan read nothing")
        self.assertEqual(benchmarked - set(members), set(), "a benchmarked crate is in no component")
        # The train maps a crate to crates/<name>/Cargo.toml, so every member's directory is its package name.
        self.assertEqual({m for m in members if packages.get(m) != m}, set(),
                         "a component names a crate that is not a package of that directory name")


class OptionalWorkflowCodexSkipTests(unittest.TestCase):
    """Optional pull-request workflows skip own codex/ pull requests and still run on the merged tree."""

    SKIP = ("${{ !(github.event_name == 'pull_request' && startsWith(github.head_ref, 'codex/') "
            "&& github.event.pull_request.head.repo.full_name == github.repository) }}")
    JOBS = {
        "bench-pipeline.yml": "pipeline-gate",
        "tz-database-audit.yml": "tz-database-audit",
        "sqlite-capacity.yml": "sqlite-capacity",
        "bench-1m.yml": "ann-ci-gate",
        "bench-track.yml": "changes",
    }
    # Each of these has a pull_request trigger with paths and a push trigger on integration/** with the same paths.
    INTEGRATION_PUSH = ("bench-pipeline.yml", "tz-database-audit.yml", "sqlite-capacity.yml", "bench-1m.yml")

    def evaluate(self, event_name, head_ref, head_repo, repository="ohdearquant/khive"):
        expression = self.SKIP.removeprefix("${{ ").removesuffix(" }}")
        for name, local in {
            "github.event.pull_request.head.repo.full_name": "head_repo",
            "github.event_name": "event_name",
            "github.head_ref": "head_ref",
            "github.repository": "repository",
        }.items():
            expression = expression.replace(name, local)
        expression = expression.replace("!(", "not (").replace("startsWith(", "starts_with(")
        expression = expression.replace("&&", " and ")
        self.assertNotRegex(expression, r"github\.|!", "an unmapped name or operator is left in the expression")

        def starts_with(value, prefix):
            # GitHub's startsWith is case-insensitive and treats null as "".
            return (value or "").lower().startswith(prefix.lower())

        scope = {"starts_with": starts_with, "event_name": event_name, "head_ref": head_ref,
                 "head_repo": head_repo, "repository": repository}
        return eval(expression, {"__builtins__": {}}, scope)

    def trigger(self, text, event):
        return indented_block(indented_block(text, "on", 0), event, 2)

    def paths(self, block):
        return [line.strip() for line in indented_block(block, "paths", 4).splitlines()
                if line.strip().startswith("- ")]

    def test_each_optional_job_carries_the_codex_skip(self):
        for workflow, job in self.JOBS.items():
            with self.subTest(workflow=workflow):
                block = indented_block(workflow_text(workflow), job, 2)
                self.assertEqual(re.findall(r"(?m)^    if: (.+)$", block), [self.SKIP])

    def test_the_skip_applies_to_own_codex_pull_requests_only(self):
        self.assertFalse(self.evaluate("pull_request", "codex/change", "ohdearquant/khive"))
        self.assertFalse(self.evaluate("pull_request", "Codex/change", "ohdearquant/khive"))
        for event_name, head_ref, head_repo in (
                ("pull_request", "codex/change", "contributor/khive"),
                ("pull_request", "codex/change", None),
                ("pull_request", "feature/codex/x", "ohdearquant/khive"),
                ("pull_request", "batch/1", "ohdearquant/khive"),
                ("push", "", None), ("workflow_dispatch", "", None)):
            with self.subTest(event_name=event_name, head_ref=head_ref, head_repo=head_repo):
                self.assertTrue(self.evaluate(event_name, head_ref, head_repo))

    def test_skipped_workflows_run_on_integration_pushes_with_their_pull_request_paths(self):
        for workflow in self.INTEGRATION_PUSH:
            with self.subTest(workflow=workflow):
                text = workflow_text(workflow)
                push = self.trigger(text, "push")
                branches = re.search(r"(?m)^    branches: \[(.*)\]$", push)
                self.assertIsNotNone(branches)
                self.assertIn('"integration/**"', branches.group(1))
                self.assertTrue(self.paths(push))
                self.assertEqual(self.paths(push), self.paths(self.trigger(text, "pull_request")))

    def test_bench_track_publishes_from_main_only(self):
        # A merge train's push runs the components of its dependents closure (ADR-206 Amendment 1). The
        # performance ledger is written from main and release tags only, so every step that publishes
        # carries that ref guard, and the e2e job, which the train does not run, excludes integration pushes.
        text = workflow_text("bench-track.yml")
        push = self.trigger(text, "push")
        self.assertEqual(re.findall(r"(?m)^    branches: (.+)$", push), ['["integration/train-*"]'])
        guard = "(github.ref == 'refs/heads/main' || startsWith(github.ref, 'refs/tags/v'))"
        for workflow in ("bench-track.yml", "bench-component.yml"):
            steps = re.split(r"(?m)^      - ", workflow_text(workflow))[1:]
            publishing = [step for step in steps if "run: bash scripts/perf/publish_ledger.sh" in step]
            with self.subTest(workflow=workflow):
                self.assertEqual(len(publishing), 1)
                condition = re.search(r"(?m)^        if: (.+)$", publishing[0])
                self.assertIsNotNone(condition)
                self.assertIn(guard, condition.group(1))
        e2e = indented_block(indented_block(text, "jobs", 0), "e2e", 2)
        self.assertIn("!(github.event_name == 'push' && startsWith(github.ref, 'refs/heads/integration/'))", e2e)


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
"""Behaviour tests for the CI infra retry step: which runs it retries, and how."""

from __future__ import annotations

import json
import os
import pathlib
import subprocess
import tempfile
import textwrap
import unittest

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
WORKFLOW = REPO_ROOT / ".github" / "workflows" / "ci-infra-retry.yml"

# A stand-in for gh: GET calls answer from the fixture files, POST calls are
# recorded by their final path segment (the rerun endpoint) and succeed.
FAKE_GH = """#!/bin/bash
for arg in "$@"; do last="$arg"; done
if [[ " $* " == *" --method POST "* ]]; then
  echo "${last##*/}" >> "$FAKE_GH_DIR/posts"
  exit 0
fi
if [[ "$last" == */attempts/*/jobs* ]]; then
  cat "$FAKE_GH_DIR/jobs.json"
else
  cat "$FAKE_GH_DIR/run.json"
fi
"""


def ran(name, conclusion="success", steps=("Set up job", "Run"), runner=7):
    return {
        "name": name,
        "status": "completed",
        "conclusion": conclusion,
        "steps": [{"name": step, "conclusion": "success"} for step in steps],
        "runner_id": runner,
        "runner_name": f"GitHub Actions {runner}",
    }


def never_ran(name, conclusion="cancelled"):
    # The shape GitHub reports for a job that waited for a runner and was
    # cancelled before one was assigned.
    return {
        "name": name,
        "status": "completed",
        "conclusion": conclusion,
        "steps": [],
        "runner_id": 0,
        "runner_name": "",
    }


def failed_in_step(name):
    job = ran(name, conclusion="failure", steps=("Set up job",))
    job["steps"].append({"name": "Run tests", "conclusion": "failure"})
    return job


class CiInfraRetryTests(unittest.TestCase):
    def setUp(self):
        text = WORKFLOW.read_text()
        step = text.split("- name: Inspect failed jobs and retry setup-only failures once\n", 1)[1]
        body = []
        for line in step.split("        run: |\n", 1)[1].splitlines():
            # The block scalar ends at the first non-blank line indented less
            # than its content; anything after it is YAML, not shell.
            if line.strip() and not line.startswith(" " * 10):
                break
            body.append(line)
        self.script = textwrap.dedent("\n".join(body))

    def run_retry(self, jobs, attempt=1):
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = pathlib.Path(tmp)
            (tmp_path / "jobs.json").write_text(json.dumps({"total_count": len(jobs), "jobs": jobs}))
            (tmp_path / "run.json").write_text(json.dumps({"run_attempt": attempt}))
            gh = tmp_path / "gh"
            gh.write_text(FAKE_GH)
            gh.chmod(0o755)
            result = subprocess.run(
                ["bash", "--noprofile", "--norc", "-eo", "pipefail", "-c", self.script],
                env={
                    **os.environ,
                    "PATH": f"{tmp}:{os.environ['PATH']}",
                    "FAKE_GH_DIR": tmp,
                    "GH_TOKEN": "unused",
                    "REPOSITORY": "owner/repo",
                    "RUN_ID": "1",
                    "GITHUB_STEP_SUMMARY": str(tmp_path / "summary"),
                },
                capture_output=True, text=True, timeout=30, check=False,
            )
            posts_file = tmp_path / "posts"
            posts = posts_file.read_text().split() if posts_file.exists() else []
            return result, posts

    def assert_retried(self, jobs, endpoint):
        result, posts = self.run_retry(jobs)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(posts, [endpoint], result.stdout + result.stderr)

    def assert_declined(self, jobs):
        result, posts = self.run_retry(jobs)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertEqual(posts, [], result.stdout + result.stderr)
        self.assertIn("No automatic retry", result.stdout)

    def test_jobs_cancelled_before_a_runner_retry_the_whole_run_once(self):
        # A leaf and the gate both waited for a runner until cancelled.
        self.assert_retried(
            [ran("Tests"), never_ran("Coverage ratchet"), never_ran("CI gate")], "rerun"
        )

    def test_gate_alone_cancelled_before_a_runner_is_retried(self):
        self.assert_retried([ran("Tests"), ran("Coverage ratchet"), never_ran("CI gate")], "rerun")

    def test_gate_failing_because_a_leaf_never_ran_does_not_block_the_retry(self):
        # CI gate's only step aggregates its needs' results; with every other
        # job green, its failure is the result of the need that never ran.
        gate = ran("CI gate", conclusion="failure")
        self.assert_retried([ran("Tests"), never_ran("Coverage ratchet"), gate], "rerun")

    def test_job_cancelled_after_it_started_is_not_retried(self):
        # Cancelled while a step was running, by hand or by a timeout reported
        # as a cancellation: repository code ran, so the run stays red.
        stopped = ran("Coverage ratchet", conclusion="cancelled")
        self.assert_declined([ran("Tests"), stopped, never_ran("CI gate")])

    def test_timed_out_job_beside_a_runnerless_cancel_is_not_retried(self):
        timed_out = ran("Tests", conclusion="timed_out")
        self.assert_declined([timed_out, never_ran("Coverage ratchet"), never_ran("CI gate")])

    def test_timed_out_job_beside_a_setup_only_failure_is_not_retried(self):
        timed_out = ran("Tests", conclusion="timed_out")
        self.assert_declined([timed_out, never_ran("Docs lint", conclusion="failure")])

    def test_any_other_end_state_rules_out_a_retry(self):
        for conclusion in ("timed_out", "startup_failure", "neutral", "action_required", None):
            with self.subTest(conclusion=conclusion):
                other = ran("Docs lint", conclusion=conclusion)
                self.assert_declined([ran("Tests"), other, never_ran("CI gate")])

    def test_job_cancelled_after_a_runner_was_assigned_is_not_retried(self):
        assigned = ran("Coverage ratchet", conclusion="cancelled", steps=())
        self.assert_declined([ran("Tests"), assigned, never_ran("CI gate")])

    def test_test_failure_beside_a_runnerless_cancel_is_not_retried(self):
        self.assert_declined(
            [failed_in_step("Tests"), never_ran("Coverage ratchet"), never_ran("CI gate")]
        )

    def test_setup_only_leaf_failure_still_reruns_failed_jobs(self):
        gate = ran("CI gate", conclusion="failure")
        self.assert_retried(
            [ran("Tests"), never_ran("Docs lint", conclusion="failure"), gate],
            "rerun-failed-jobs",
        )

    def test_gate_failing_with_every_leaf_green_is_not_retried(self):
        gate = failed_in_step("CI gate")
        self.assert_declined([ran("Tests"), ran("Coverage ratchet"), gate])

    def test_second_attempt_is_never_retried(self):
        result, posts = self.run_retry(
            [ran("Tests"), never_ran("Coverage ratchet"), never_ran("CI gate")], attempt=2
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(posts, [])
        self.assertIn("retry budget is exhausted", result.stdout)


if __name__ == "__main__":
    unittest.main()

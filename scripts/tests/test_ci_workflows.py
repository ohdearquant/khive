#!/usr/bin/env python3
"""Contract tests for CI workflow triggers, permissions, and command wiring."""

from __future__ import annotations

import json
import os
import pathlib
import subprocess
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


class AutoMergeGuardWorkflowTests(unittest.TestCase):
    def test_push_guard_has_only_required_write_permissions(self):
        workflow = workflow_text("ci.yml")
        guard = indented_block(workflow, "automerge-push-guard", 2)
        permissions = mapping_entries(indented_block(guard, "permissions", 4))
        self.assertEqual(permissions, {"contents: write", "pull-requests: write"})


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
                "if [ \"${1:-}\" = view ]; then exit 1; fi\n"
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


if __name__ == "__main__":
    unittest.main()

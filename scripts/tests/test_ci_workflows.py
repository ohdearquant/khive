#!/usr/bin/env python3
"""Contract tests for CI workflow triggers, permissions, and command wiring."""

from __future__ import annotations

import json
import os
import pathlib
import re
import shlex
import subprocess
import sys
import tempfile
import textwrap
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
            for rc in (0, 124, 137):
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
                    if phase == "compile":
                        self.assertEqual(output.read_text(), f"exit_code={rc}\n")

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


if __name__ == "__main__":
    unittest.main()

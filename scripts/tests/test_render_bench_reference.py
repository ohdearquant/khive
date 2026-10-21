#!/usr/bin/env python3
"""Tests for the benchmark-reference renderer."""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import pathlib
import subprocess
import tempfile
import unittest

SCRIPT = pathlib.Path(__file__).resolve().parents[1] / "render-bench-reference.py"
SPEC = importlib.util.spec_from_file_location("render_bench_reference", SCRIPT)
renderer = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(renderer)


def git(repo, *args):
    result = subprocess.run(
        ["git", "-C", str(repo), *args],
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise AssertionError(f"git {' '.join(args)} failed: {result.stderr}")
    return result.stdout.strip()


class Fixture:
    def __init__(self, root):
        self.root = pathlib.Path(root)
        self.crate = "khive-example"
        self.reference = self.root / "crates" / self.crate / "perf" / "reference.json"
        self.document = self.root / "crates" / self.crate / "docs" / "benchmarks.md"
        self.reference.parent.mkdir(parents=True)
        self.document.parent.mkdir(parents=True)

    def commit(self, message, date):
        git(self.root, "add", "--all")
        subprocess.run(
            ["git", "-C", str(self.root), "commit", "-q", "-m", message],
            check=True,
            env={
                **__import__("os").environ,
                "GIT_AUTHOR_DATE": f"{date}T12:00:00+00:00",
                "GIT_COMMITTER_DATE": f"{date}T12:00:00+00:00",
                "GIT_CONFIG_NOSYSTEM": "1",
                "GIT_CONFIG_GLOBAL": str(self.root / "no-gitconfig"),
            },
        )
        return git(self.root, "rev-parse", "HEAD")

    def write_reference(self, payload):
        self.reference.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n")

    def write_document(self, text):
        self.document.write_text(text)

    def v1(self, sha, benches):
        return {
            "schema": "bench-reference/v1",
            "crate": self.crate,
            "sha": sha,
            "benches": benches,
        }


def measured(median_ns, aa_spread=1.001):
    return {
        "median_ns": median_ns,
        "aa_spread": aa_spread,
        "verdict": "MEASURED",
    }


def unmeasured():
    return {"verdict": "UNMEASURED"}


def quiet_main(*args):
    stdout, stderr = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
        code = renderer.main([*args])
    return code, stdout.getvalue(), stderr.getvalue()


class RendererTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="bench-doc-render-")
        self.addCleanup(self.temporary.cleanup)
        self.root = pathlib.Path(self.temporary.name)
        (self.root / "crates").mkdir()
        git(self.root, "init", "--initial-branch=main")
        git(self.root, "config", "user.name", "Renderer Fixture")
        git(self.root, "config", "user.email", "renderer@example.invalid")
        git(self.root, "config", "commit.gpgsign", "false")
        self.fixture = Fixture(self.root)
        self.fixture.write_reference(
            {"schema": "bench-reference/v1", "crate": "khive-example", "sha": "0" * 40, "benches": {}}
        )
        self.fixture.write_document("# Benchmarks\n\nHand-written prose.\n")
        self.sha = self.fixture.commit("add fixture files", "2026-01-02")
        self.fixture.write_reference(self.fixture.v1(self.sha, {}))

    def write_block(self, payload):
        self.fixture.write_reference(payload)
        rendered = renderer.render_document(
            self.fixture.reference, self.fixture.document, self.root
        )
        self.fixture.write_document(rendered)

    def test_writes_measured_rows_with_unit_ratio_and_commit_date(self):
        self.fixture.write_document(
            f"# Benchmarks\n\nBefore.\n\n{renderer.BEGIN_MARKER}\nold\n{renderer.END_MARKER}\n\nAfter.\n"
        )
        self.write_block(
            self.fixture.v1(self.sha, {"search/query": measured(1_234_567.891)})
        )
        text = self.fixture.document.read_text()
        self.assertIn("Before.", text)
        self.assertIn("After.", text)
        row = next(line for line in text.splitlines() if "`search/query`" in line)
        self.assertEqual(
            [cell.strip() for cell in row.strip("|").split("|")][1:3],
            ["1.235 ms", "1.001"],
        )
        self.assertIn(self.sha, text)
        self.assertIn("2026-01-02", text)
        self.assertNotIn("old", text)

    def test_unmeasured_row_is_never_rendered_as_a_number(self):
        self.fixture.write_document(
            f"{renderer.BEGIN_MARKER}\n{renderer.END_MARKER}\n"
        )
        self.write_block(
            self.fixture.v1(
                self.sha,
                {"search/query": measured(2_000.0), "search/absent": unmeasured()},
            )
        )
        text = self.fixture.document.read_text()
        row = next(line for line in text.splitlines() if "`search/absent`" in line)
        self.assertEqual(
            [cell.strip() for cell in row.strip("|").split("|")][1:3],
            ["unmeasured", "unmeasured"],
        )
        self.assertNotRegex(row, r"\d+\.\d+ (ns|µs|ms|s)")

    def test_missing_markers_are_reported_and_document_is_untouched(self):
        original = "# Benchmarks\n\nNo markers here.\n"
        self.fixture.write_document(original)
        self.fixture.write_reference(
            self.fixture.v1(self.sha, {"search/query": measured(1000.0)})
        )
        code, stdout, stderr = quiet_main("--write", "--root", str(self.root))
        self.assertEqual(code, 1)
        self.assertEqual(self.fixture.document.read_text(), original)
        self.assertIn("has no", stderr)

    def test_duplicate_markers_are_refused(self):
        duplicate = (
            f"{renderer.BEGIN_MARKER}\n{renderer.END_MARKER}\n"
            f"{renderer.BEGIN_MARKER}\n{renderer.END_MARKER}\n"
        )
        self.fixture.write_document(duplicate)
        self.fixture.write_reference(
            self.fixture.v1(self.sha, {"search/query": measured(1000.0)})
        )
        code, _, stderr = quiet_main("--check", "--root", str(self.root))
        self.assertEqual(code, 1)
        self.assertIn("exactly one", stderr)

    def test_write_is_idempotent(self):
        self.fixture.write_document(
            f"{renderer.BEGIN_MARKER}\n{renderer.END_MARKER}\n"
        )
        self.fixture.write_reference(
            self.fixture.v1(self.sha, {"search/query": measured(1000.0)})
        )
        self.assertEqual(quiet_main("--write", "--root", str(self.root))[0], 0)
        first = self.fixture.document.read_text()
        self.assertEqual(quiet_main("--write", "--root", str(self.root))[0], 0)
        self.assertEqual(self.fixture.document.read_text(), first)

    def test_check_fails_then_passes_after_write(self):
        self.fixture.write_document(
            f"{renderer.BEGIN_MARKER}\nstale\n{renderer.END_MARKER}\n"
        )
        self.fixture.write_reference(
            self.fixture.v1(self.sha, {"search/query": measured(1000.0)})
        )
        code, _, stderr = quiet_main("--check", "--root", str(self.root))
        self.assertEqual(code, 1)
        self.assertIn("stale", stderr)
        self.assertEqual(quiet_main("--write", "--root", str(self.root))[0], 0)
        self.assertEqual(quiet_main("--check", "--root", str(self.root))[0], 0)

    def test_malformed_json_fails_loudly(self):
        self.fixture.write_document(
            f"{renderer.BEGIN_MARKER}\n{renderer.END_MARKER}\n"
        )
        self.fixture.reference.write_text('{"schema": "bench-reference/v1",\n')
        code, _, stderr = quiet_main("--check", "--root", str(self.root))
        self.assertEqual(code, 1)
        self.assertIn("not valid JSON", stderr)

    def test_invalid_measured_value_fails_loudly(self):
        self.fixture.write_document(
            f"{renderer.BEGIN_MARKER}\n{renderer.END_MARKER}\n"
        )
        self.fixture.write_reference(
            self.fixture.v1(self.sha, {"search/query": {"verdict": "MEASURED"}})
        )
        code, _, stderr = quiet_main("--check", "--root", str(self.root))
        self.assertEqual(code, 1)
        self.assertIn("median_ns", stderr)

    def test_rows_are_sorted_bytewise(self):
        self.fixture.write_document(
            f"{renderer.BEGIN_MARKER}\n{renderer.END_MARKER}\n"
        )
        self.write_block(
            self.fixture.v1(
                self.sha,
                {
                    "z/last": measured(2000.0),
                    "a/first": measured(1000.0),
                    "m/middle": measured(3000.0),
                },
            )
        )
        positions = [
            self.fixture.document.read_text().index(name)
            for name in ("`a/first`", "`m/middle`", "`z/last`")
        ]
        self.assertEqual(positions, sorted(positions))

    def test_measured_without_aa_ratio_fails_loudly(self):
        self.fixture.write_document(
            f"{renderer.BEGIN_MARKER}\n{renderer.END_MARKER}\n"
        )
        self.fixture.write_reference(
            self.fixture.v1(
                self.sha,
                {"search/query": {"median_ns": 1000.0, "verdict": "MEASURED"}},
            )
        )
        code, _, stderr = quiet_main("--check", "--root", str(self.root))
        self.assertEqual(code, 1)
        self.assertIn("aa_spread", stderr)

    def test_v2_entries_use_each_entry_commit_and_target(self):
        self.fixture.write_reference(
            {"schema": "bench-reference/v1", "crate": self.fixture.crate, "sha": self.sha, "benches": {}}
        )
        second_sha = self.fixture.commit("second baseline", "2026-02-03")
        self.fixture.write_document(
            f"{renderer.BEGIN_MARKER}\n{renderer.END_MARKER}\n"
        )
        self.write_block(
            {
                "schema": "bench-reference/v2",
                "crate": self.fixture.crate,
                "targets": {
                    "bench_target": {
                        "entries": [
                            {
                                "commit": self.sha,
                                "benchmarks": {
                                    "scenario": {
                                        "disposition": "MEASURED",
                                        "median_ns": 1234.0,
                                        "aa_spread": 1.002,
                                    }
                                },
                            },
                            {
                                "commit": second_sha,
                                "benchmarks": {
                                    "scenario": {
                                        "disposition": "UNMEASURED",
                                        "reason": "withheld",
                                    }
                                },
                            },
                        ]
                    }
                },
            }
        )
        text = self.fixture.document.read_text()
        self.assertIn(self.sha, text)
        self.assertIn(second_sha, text)
        self.assertIn("2026-01-02", text)
        self.assertIn("2026-02-03", text)
        rows = [line for line in text.splitlines() if "`bench_target/scenario`" in line]
        self.assertEqual(len(rows), 2)
        self.assertEqual(
            [cell.strip() for cell in rows[0].strip("|").split("|")][1:3],
            ["1.234 µs", "1.002"],
        )
        self.assertEqual(
            [cell.strip() for cell in rows[1].strip("|").split("|")][1:3],
            ["unmeasured", "unmeasured"],
        )

    def test_v2_benchmarks_keyed_by_scenario_then_metric(self):
        self.fixture.write_reference(
            {"schema": "bench-reference/v1", "crate": self.fixture.crate, "sha": self.sha, "benches": {}}
        )
        self.fixture.write_document(
            f"{renderer.BEGIN_MARKER}\n{renderer.END_MARKER}\n"
        )
        self.write_block(
            {
                "schema": "bench-reference/v2",
                "crate": self.fixture.crate,
                "targets": {
                    "bench_target": {
                        "entries": [
                            {
                                "commit": self.sha,
                                "benchmarks": {
                                    "scenario": {
                                        "median": {
                                            "disposition": "MEASURED",
                                            "median_ns": 2500.0,
                                            "aa_spread": 1.004,
                                        },
                                        "allocations": {
                                            "disposition": "MEASURED",
                                            "value": 17.0,
                                            "unit": "allocs/op",
                                            "aa_spread": 1.0,
                                        },
                                        "rss": {
                                            "disposition": "UNMEASURED",
                                            "reason": "not repeatable",
                                        },
                                    }
                                },
                            }
                        ]
                    }
                },
            }
        )
        text = self.fixture.document.read_text()
        rows = {
            cells[0].strip("`"): cells[1:]
            for cells in (
                [cell.strip() for cell in line.strip("|").split("|")]
                for line in text.splitlines()
                if line.startswith("| `bench_target/scenario/")
            )
        }
        self.assertEqual(
            set(rows),
            {
                "bench_target/scenario/allocations",
                "bench_target/scenario/median",
                "bench_target/scenario/rss",
            },
        )
        self.assertEqual(rows["bench_target/scenario/median"][0:2], ["2.500 µs", "1.004"])
        self.assertEqual(
            rows["bench_target/scenario/allocations"][0:2], ["17.000 allocs/op", "1.000"]
        )
        self.assertEqual(
            rows["bench_target/scenario/rss"][0:2], ["unmeasured", "unmeasured"]
        )


if __name__ == "__main__":
    unittest.main()

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


COMPARISON_CLASS = {
    "cores": "2,3",
    "cpu": "Example CPU 8C",
    "cpufreq": {"governor": ["performance"]},
    "kernel": "6.8.0-1-generic",
    "profile": "bench",
    "rustc": "rustc 1.95.0 (59807616e 2026-04-14)",
}


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
            "class": COMPARISON_CLASS,
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


def criterion_row(median_ns, aa_spread):
    return {
        "a": {"median_ns": median_ns},
        "aa_spread": aa_spread,
        "b": {"median_ns": median_ns},
        "kind": "criterion",
        "median_ns": median_ns,
        "metric": "median_ns",
        "verdict": "MEASURED",
    }


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

    def test_block_names_the_comparison_class(self):
        self.fixture.write_document(
            f"{renderer.BEGIN_MARKER}\n{renderer.END_MARKER}\n"
        )
        self.write_block(
            self.fixture.v1(self.sha, {"search/query": measured(1000.0)})
        )
        self.assertIn(
            "Comparison class: `rustc 1.95.0 (59807616e 2026-04-14)`, `bench` profile, "
            "Example CPU 8C, cores 2,3, kernel 6.8.0-1-generic.",
            self.fixture.document.read_text(),
        )

    def test_reference_without_a_complete_comparison_class_is_refused(self):
        document = f"{renderer.BEGIN_MARKER}\n{renderer.END_MARKER}\n"
        incomplete = {key: value for key, value in COMPARISON_CLASS.items() if key != "kernel"}
        for label, payload, expected in (
            ("no class", {"class": None}, "class must be"),
            ("no kernel", {"class": incomplete}, "class.kernel"),
        ):
            with self.subTest(label):
                self.fixture.write_document(document)
                reference = self.fixture.v1(self.sha, {"search/query": measured(1000.0)})
                reference.update(payload)
                if reference["class"] is None:
                    del reference["class"]
                self.fixture.write_reference(reference)
                code, _, stderr = quiet_main("--write", "--root", str(self.root))
                self.assertEqual(code, 1)
                self.assertIn(expected, stderr)
                self.assertEqual(self.fixture.document.read_text(), document)

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

    def v2(self, entries):
        return {
            "schema": "bench-reference/v2",
            "crate": self.fixture.crate,
            "class": COMPARISON_CLASS,
            "targets": {
                "khive-example:bench_target": {"entries": entries},
                "khive-example:not_run": {"absence": "excluded", "reason": "excluded by the job"},
            },
        }

    def rows(self, prefix):
        text = self.fixture.document.read_text()
        return [
            [cell.strip() for cell in line.strip("|").split("|")]
            for line in text.splitlines()
            if line.startswith(f"| `{prefix}")
        ]

    def test_v2_entries_use_each_entry_commit_and_target(self):
        self.fixture.write_reference(
            {"schema": "bench-reference/v1", "crate": self.fixture.crate, "sha": self.sha, "benches": {}}
        )
        second_sha = self.fixture.commit("second baseline", "2026-02-03")
        self.fixture.write_document(
            f"{renderer.BEGIN_MARKER}\n{renderer.END_MARKER}\n"
        )
        self.write_block(
            self.v2(
                [
                    {"commit": self.sha, "benchmarks": {"scenario": {"median_ns": criterion_row(1234.0, 1.002)}}},
                    {
                        "commit": second_sha,
                        "benchmarks": {
                            "scenario": {
                                "median_ns": {
                                    "kind": "criterion",
                                    "metric": "median_ns",
                                    "verdict": "UNMEASURED",
                                    "reasons": ["A/A spread 1.200 > 1.1"],
                                }
                            }
                        },
                    },
                ]
            )
        )
        text = self.fixture.document.read_text()
        self.assertIn("2026-01-02", text)
        self.assertIn("2026-02-03", text)
        self.assertNotIn("not_run", text)
        rows = self.rows("khive-example:bench_target/scenario/median_ns`")
        self.assertEqual([row[1:5] for row in rows], [
            ["1.234 µs", "1.002", f"`{self.sha}`", "2026-01-02"],
            ["unmeasured", "unmeasured", f"`{second_sha}`", "2026-02-03"],
        ])

    def test_v2_renders_every_record_kind_the_producer_writes(self):
        self.fixture.write_document(
            f"{renderer.BEGIN_MARKER}\n{renderer.END_MARKER}\n"
        )
        # The first two records are copied from a reference file the producer wrote.
        self.write_block(
            self.v2(
                [
                    {
                        "commit": self.sha,
                        "benchmarks": {
                            "changeset_invert/entity_create/1": {
                                "median_ns": {
                                    "a": {"ci_hi_ns": 500.8366205434031, "ci_lo_ns": 500.08121625861315,
                                          "median_ns": 500.34433984370423},
                                    "aa_spread": 1.0613,
                                    "b": {"ci_hi_ns": 472.20940399838946, "ci_lo_ns": 469.99217542146573,
                                          "median_ns": 471.4651172854575},
                                    "kind": "criterion",
                                    "median_ns": 485.905,
                                    "metric": "median_ns",
                                    "verdict": "MEASURED",
                                }
                            },
                            "p95_gate/synthetic-v1/m1": {
                                "p95_ns": {
                                    "a": {"value": 4635000.0},
                                    "aa_spread": 1.0047,
                                    "b": {"value": 4657000.0},
                                    "kind": "bench-json",
                                    "metric": "p95_ns",
                                    "n": 200,
                                    "statistic": "p95_ns",
                                    "value": 4646000.0,
                                    "verdict": "MEASURED",
                                }
                            },
                            "quality/recall": {
                                "recall_at_10": {
                                    "a": {"value": 0.95},
                                    "b": {"value": 0.95},
                                    "kind": "bench-json",
                                    "metric": "recall_at_10",
                                    "n": 200,
                                    "statistic": "recall_at_10",
                                    "value": 0.95,
                                    "verdict": "MEASURED",
                                }
                            },
                            "classify_allocations": {
                                "report": {
                                    "kind": "allocation",
                                    "metric": "report",
                                    "report": {"allocations": 0, "calls": 1000000, "scenario": "classify_allocations"},
                                    "verdict": "MEASURED",
                                }
                            },
                            "legacy/converted": {
                                "median_ns": {"a": {}, "aa_spread": 1.0005, "b": {}, "median_ns": 39475054.485,
                                              "verdict": "MEASURED"}
                            },
                        },
                    }
                ]
            )
        )
        rows = {row[0].strip("`"): row[1:3] for row in self.rows("khive-example:bench_target/")}
        prefix = "khive-example:bench_target/"
        self.assertEqual(rows, {
            prefix + "changeset_invert/entity_create/1/median_ns": ["485.905 ns", "1.061"],
            prefix + "p95_gate/synthetic-v1/m1/p95_ns": ["4.646 ms", "1.005"],
            prefix + "quality/recall/recall_at_10": ["0.95", "n/a"],
            prefix + "classify_allocations/report": ["allocations=0, calls=1000000", "n/a"],
            prefix + "legacy/converted/median_ns": ["39.475 ms", "1.000"],
        })

    def test_v2_record_without_a_verdict_is_refused(self):
        self.fixture.write_document(
            f"{renderer.BEGIN_MARKER}\n{renderer.END_MARKER}\n"
        )
        record = criterion_row(1234.0, 1.002)
        record["disposition"] = record.pop("verdict")
        self.fixture.write_reference(
            self.v2([{"commit": self.sha, "benchmarks": {"scenario": {"median_ns": record}}}])
        )
        code, _, stderr = quiet_main("--check", "--root", str(self.root))
        self.assertEqual(code, 1)
        self.assertIn("['median_ns'].verdict must be MEASURED or UNMEASURED", stderr)

    def test_v2_record_not_keyed_by_metric_is_refused(self):
        self.fixture.write_document(
            f"{renderer.BEGIN_MARKER}\n{renderer.END_MARKER}\n"
        )
        self.fixture.write_reference(
            self.v2([{"commit": self.sha, "benchmarks": {"scenario": criterion_row(1234.0, 1.002)}}])
        )
        code, _, stderr = quiet_main("--check", "--root", str(self.root))
        self.assertEqual(code, 1)
        self.assertIn("['scenario'] must be keyed by metric", stderr)

    def test_v2_timing_without_aa_ratio_and_unknown_kind_are_refused(self):
        timing = {"kind": "bench-json", "metric": "p95_ns", "n": 200, "statistic": "p95_ns",
                  "value": 4646000.0, "verdict": "MEASURED"}
        unknown = {"kind": "flamegraph", "metric": "report", "verdict": "MEASURED"}
        for record, message in ((timing, "['p95_ns'].aa_spread must be a number"),
                                (unknown, "kind 'flamegraph' is not criterion, bench-json or allocation")):
            with self.subTest(message=message):
                self.fixture.write_document(
                    f"{renderer.BEGIN_MARKER}\n{renderer.END_MARKER}\n"
                )
                self.fixture.write_reference(
                    self.v2([{"commit": self.sha, "benchmarks": {"scenario": {record["metric"]: record}}}])
                )
                code, _, stderr = quiet_main("--check", "--root", str(self.root))
                self.assertEqual(code, 1)
                self.assertIn(message, stderr)

if __name__ == "__main__":
    unittest.main()

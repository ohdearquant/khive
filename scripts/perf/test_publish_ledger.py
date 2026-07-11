#!/usr/bin/env python3
"""Unit tests for scripts/perf/publish_ledger.sh (stdlib unittest, drives the
real script against a scratch local git remote - no network).

Run: python3 -m unittest scripts.perf.test_publish_ledger -v
     (or: cd scripts/perf && python3 -m unittest test_publish_ledger -v)
"""

from __future__ import annotations

import json
import pathlib
import subprocess
import tempfile
import unittest

SCRIPT = pathlib.Path(__file__).parent / "publish_ledger.sh"


def _run(argv, cwd):
    return subprocess.run(argv, cwd=str(cwd), capture_output=True, text=True, check=False)


class PublishLedgerHistoryTests(unittest.TestCase):
    """Reproduces the round-trip a real bench-track.yml run performs: two
    sequential publishes of the SAME suite ledger file, each starting from a
    fresh local `bench-data/<suite>.jsonl` that holds only that run's own
    record (a real CI run's plain main checkout never sees perf-data's
    history before append_record() writes it). Both records must still be
    present on the `perf-data` branch afterwards - the bug this guards
    against was `publish_ledger.sh` `cp`-ing the local (single-record) file
    straight over the worktree's (full-history) file, silently discarding
    every prior run's data on every publish.
    """

    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        root = pathlib.Path(self._tmp.name)

        self.origin = root / "origin.git"
        subprocess.run(["git", "init", "--bare", "-q", str(self.origin)], check=True)

        self.work = root / "work"
        self.work.mkdir()
        subprocess.run(["git", "init", "-q"], cwd=self.work, check=True)
        subprocess.run(["git", "config", "user.name", "test"], cwd=self.work, check=True)
        subprocess.run(["git", "config", "user.email", "test@example.com"], cwd=self.work, check=True)
        (self.work / "README.md").write_text("scratch repo for publish_ledger.sh test\n")
        subprocess.run(["git", "add", "README.md"], cwd=self.work, check=True)
        subprocess.run(["git", "commit", "-q", "-m", "init"], cwd=self.work, check=True)
        subprocess.run(["git", "remote", "add", "origin", str(self.origin)], cwd=self.work, check=True)

    def _publish(self, content: str):
        """Simulate one CI run: overwrite the local bench-data/<suite>.jsonl
        with ONLY this run's record (mirroring append_record()'s output on a
        runner that never fetched perf-data), then invoke the real publish
        script exactly as the workflow does.
        """
        rel = "bench-data/components.jsonl"
        path = self.work / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)
        result = _run(["bash", str(SCRIPT), rel], cwd=self.work)
        self.assertEqual(result.returncode, 0, msg=f"stdout={result.stdout}\nstderr={result.stderr}")

    def _fetch_ledger(self) -> str:
        subprocess.run(["git", "fetch", "-q", "origin", "perf-data"], cwd=self.work, check=True)
        out = subprocess.run(
            ["git", "show", "origin/perf-data:bench-data/components.jsonl"],
            cwd=self.work,
            capture_output=True,
            text=True,
            check=True,
        )
        return out.stdout

    def test_two_sequential_publishes_both_survive(self):
        record_a = json.dumps({"schema_version": 1, "suite": "components", "sha": "a" * 40}, sort_keys=True)
        record_b = json.dumps({"schema_version": 1, "suite": "components", "sha": "b" * 40}, sort_keys=True)

        self._publish(record_a + "\n")
        lines_after_first = self._fetch_ledger().splitlines()
        self.assertEqual(lines_after_first, [record_a])

        self._publish(record_b + "\n")
        lines_after_second = self._fetch_ledger().splitlines()

        self.assertIn(record_a, lines_after_second, "first publish's record was overwritten, not preserved")
        self.assertIn(record_b, lines_after_second)
        self.assertEqual(len(lines_after_second), 2)

    def test_republishing_identical_content_does_not_duplicate(self):
        record = json.dumps({"schema_version": 1, "suite": "components", "sha": "c" * 40}, sort_keys=True)
        self._publish(record + "\n")
        self._publish(record + "\n")  # e.g. a re-run of the same job at the same commit
        lines = self._fetch_ledger().splitlines()
        self.assertEqual(lines, [record])

    def test_three_sequential_publishes_all_survive(self):
        records = [
            json.dumps({"schema_version": 1, "suite": "components", "sha": c * 40}, sort_keys=True)
            for c in ("d", "e", "f")
        ]
        for record in records:
            self._publish(record + "\n")

        lines = self._fetch_ledger().splitlines()
        self.assertEqual(lines, records)


if __name__ == "__main__":
    unittest.main()

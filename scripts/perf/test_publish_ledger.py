#!/usr/bin/env python3
"""Unit tests for scripts/perf/publish_ledger.sh (stdlib unittest, drives the
real script against a scratch local git remote - no network).

Run: python3 -m unittest scripts.perf.test_publish_ledger -v
     (or: cd scripts/perf && python3 -m unittest test_publish_ledger -v)
"""

from __future__ import annotations

import json
import os
import pathlib
import shlex
import shutil
import subprocess
import sys
import tempfile
import unittest

SCRIPT = pathlib.Path(__file__).parent / "publish_ledger.sh"
sys.path.insert(0, str(SCRIPT.parent))
import ledger_shards  # noqa: E402


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
        return "".join(
            subprocess.run(
                ["git", "show", f"origin/perf-data:{path}"],
                cwd=self.work,
                capture_output=True,
                text=True,
                check=True,
            ).stdout
            for path in self._remote_component_paths()
        )

    def _remote_component_paths(self) -> list[str]:
        out = subprocess.run(
            ["git", "ls-tree", "-r", "--name-only", "origin/perf-data", "bench-data"],
            cwd=self.work,
            capture_output=True,
            text=True,
            check=True,
        )
        paths = out.stdout.splitlines()
        shards = sorted(path for path in paths if path.startswith("bench-data/components/"))
        legacy = ["bench-data/components.jsonl"] if "bench-data/components.jsonl" in paths else []
        return shards + legacy

    def _seed_legacy(self, content: str) -> None:
        branch = subprocess.check_output(
            ["git", "branch", "--show-current"], cwd=self.work, text=True
        ).strip()
        subprocess.run(["git", "checkout", "--orphan", "perf-data"], cwd=self.work, check=True, capture_output=True)
        subprocess.run(["git", "rm", "-rf", "--quiet", "."], cwd=self.work, check=True)
        path = self.work / "bench-data/components.jsonl"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)
        subprocess.run(["git", "add", "bench-data/components.jsonl"], cwd=self.work, check=True)
        subprocess.run(
            ["git", "commit", "-q", "-m", "legacy"],
            cwd=self.work,
            env={**os.environ, "KHIVE_ALLOW_DATA": "1"},
            check=True,
        )
        subprocess.run(["git", "push", "-q", "origin", "HEAD:perf-data"], cwd=self.work, check=True)
        subprocess.run(["git", "checkout", "-q", branch], cwd=self.work, check=True)

    def test_two_sequential_publishes_both_survive(self):
        record_a = json.dumps({"schema_version": 1, "suite": "components", "sha": "a" * 40}, sort_keys=True)
        record_b = json.dumps({"schema_version": 1, "suite": "components", "sha": "b" * 40}, sort_keys=True)

        self._publish(record_a + "\n")
        lines_after_first = self._fetch_ledger().splitlines()
        self.assertEqual(lines_after_first, [record_a])
        self.assertEqual(self._remote_component_paths(), ["bench-data/components/000001.jsonl"])

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

    def test_first_publish_migrates_legacy_flat_file_without_losing_history(self):
        old = [
            json.dumps({"schema_version": 1, "suite": "components", "sha": char * 40}, sort_keys=True)
            for char in ("g", "h")
        ]
        new = json.dumps({"schema_version": 1, "suite": "components", "sha": "i" * 40}, sort_keys=True)
        self._seed_legacy("\n".join(old) + "\n")

        self._publish(new + "\n")
        self.assertEqual(self._fetch_ledger().splitlines(), [*old, new])
        self.assertEqual(self._remote_component_paths(), ["bench-data/components/000001.jsonl"])

    def test_rollover_caps_each_shard_and_keeps_append_order(self):
        with tempfile.TemporaryDirectory() as tmp:
            data_dir = pathlib.Path(tmp) / "bench-data"
            data_dir.mkdir()
            lines = [
                json.dumps({"schema_version": 1, "suite": "components", "sha": char * 40}, sort_keys=True) + "\n"
                for char in ("a", "b", "c", "d")
            ]
            (data_dir / "components.jsonl").write_text("".join(lines[:3]))
            source = pathlib.Path(tmp) / "new.jsonl"
            source.write_text(lines[3])
            cap = 2 * len(lines[0].encode())

            self.assertEqual(ledger_shards.merge_components(source, data_dir, max_shard_bytes=cap), (3, 1))
            shards = ledger_shards.component_shard_paths(data_dir)
            self.assertEqual([path.name for path in shards], ["000001.jsonl", "000002.jsonl"])
            self.assertTrue(all(path.stat().st_size <= cap for path in shards))
            self.assertFalse((data_dir / "components.jsonl").exists())
            self.assertEqual("".join(path.read_text() for path in shards), "".join(lines))
            self.assertEqual(ledger_shards.merge_components(source, data_dir, max_shard_bytes=cap), (0, 0))

    def test_old_publisher_flat_tail_is_migrated_once_after_shards(self):
        with tempfile.TemporaryDirectory() as tmp:
            data_dir = pathlib.Path(tmp) / "bench-data"
            shard_dir = data_dir / "components"
            shard_dir.mkdir(parents=True)
            old = json.dumps({"suite": "components", "sha": "a" * 40}, sort_keys=True) + "\n"
            new = json.dumps({"suite": "components", "sha": "b" * 40}, sort_keys=True) + "\n"
            (shard_dir / "000001.jsonl").write_text(old)
            (data_dir / "components.jsonl").write_text(new)
            source = pathlib.Path(tmp) / "incoming.jsonl"
            source.write_text(new)

            self.assertEqual(ledger_shards.merge_components(source, data_dir), (1, 0))
            self.assertFalse((data_dir / "components.jsonl").exists())
            self.assertEqual((shard_dir / "000001.jsonl").read_text(), old + new)

    def test_one_rejected_push_retries_from_remote_without_duplicate(self):
        first = json.dumps({"schema_version": 1, "suite": "components", "sha": "l" * 40}, sort_keys=True)
        second = json.dumps({"schema_version": 1, "suite": "components", "sha": "m" * 40}, sort_keys=True)
        self._publish(first + "\n")
        (self.work / "bench-data/components.jsonl").write_text(second + "\n")

        bin_dir = pathlib.Path(self._tmp.name) / "retry-bin"
        bin_dir.mkdir()
        real_git = shutil.which("git")
        self.assertIsNotNone(real_git)
        git_wrapper = bin_dir / "git"
        git_wrapper.write_text(
            "#!/bin/sh\n"
            'if [ "$1" = "-C" ] && [ "$3" = "push" ] && [ ! -f "$PUSH_REJECTED" ]; then\n'
            '  : > "$PUSH_REJECTED"\n  exit 1\nfi\n'
            f"exec {shlex.quote(real_git)} \"$@\"\n"
        )
        git_wrapper.chmod(0o755)
        sleep_wrapper = bin_dir / "sleep"
        sleep_wrapper.write_text("#!/bin/sh\nexit 0\n")
        sleep_wrapper.chmod(0o755)
        output = pathlib.Path(self._tmp.name) / "retry-output"
        env = {
            **os.environ,
            "PATH": f"{bin_dir}:{os.environ['PATH']}",
            "PUSH_REJECTED": str(pathlib.Path(self._tmp.name) / "push-rejected"),
            "GITHUB_OUTPUT": str(output),
        }
        result = subprocess.run(
            ["bash", str(SCRIPT), "bench-data/components.jsonl"],
            cwd=self.work,
            env=env,
            capture_output=True,
            text=True,
            timeout=15,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("pushed on attempt 2", result.stdout)
        self.assertIn("publish_status=published", output.read_text())
        self.assertEqual(self._fetch_ledger().splitlines(), [first, second])

    def test_exhausted_push_race_is_advisory_and_marks_failure(self):
        first = json.dumps({"schema_version": 1, "suite": "components", "sha": "j" * 40}, sort_keys=True)
        lost = json.dumps({"schema_version": 1, "suite": "components", "sha": "k" * 40}, sort_keys=True)
        self._publish(first + "\n")
        (self.work / "bench-data/components.jsonl").write_text(lost + "\n")

        bin_dir = pathlib.Path(self._tmp.name) / "bin"
        bin_dir.mkdir()
        real_git = shutil.which("git")
        self.assertIsNotNone(real_git)
        git_wrapper = bin_dir / "git"
        git_wrapper.write_text(
            "#!/bin/sh\n"
            'if [ "$1" = "-C" ] && [ "$3" = "push" ]; then exit 1; fi\n'
            f"exec {shlex.quote(real_git)} \"$@\"\n"
        )
        git_wrapper.chmod(0o755)
        sleep_wrapper = bin_dir / "sleep"
        sleep_wrapper.write_text('#!/bin/sh\nprintf "%s\\n" "$1" >> "$PUBLISH_SLEEP_LOG"\n')
        sleep_wrapper.chmod(0o755)
        output = pathlib.Path(self._tmp.name) / "github-output"
        sleep_log = pathlib.Path(self._tmp.name) / "sleep-log"
        env = {
            **os.environ,
            "PATH": f"{bin_dir}:{os.environ['PATH']}",
            "GITHUB_OUTPUT": str(output),
            "PUBLISH_SLEEP_LOG": str(sleep_log),
        }
        result = subprocess.run(
            ["bash", str(SCRIPT), "bench-data/components.jsonl"],
            cwd=self.work,
            env=env,
            capture_output=True,
            text=True,
            timeout=15,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("publish_status=failed", output.read_text())
        self.assertIn("failed to push after 5 attempts", result.stderr)
        delays = [int(value) for value in sleep_log.read_text().splitlines()]
        self.assertEqual(len(delays), 4)
        self.assertTrue(all(3 * attempt <= delay <= 3 * attempt + 3 for attempt, delay in enumerate(delays, 1)))
        self.assertEqual(self._fetch_ledger().splitlines(), [first])


if __name__ == "__main__":
    unittest.main()

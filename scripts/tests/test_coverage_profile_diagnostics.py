#!/usr/bin/env python3
"""The merge forwarder excludes unreadable profiles, visibly, up to a floor."""

import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import unittest


REPO_ROOT = Path(__file__).resolve().parents[2]
HELPER = REPO_ROOT / "scripts/coverage-profile-diagnostics.py"

# Stands in for llvm-profdata: `show` rejects a file whose content starts with
# BAD, and `merge` records the input list it was handed.
FAKE_PROFDATA = """#!/bin/sh
if [ "$1" = show ]; then
  if head -c 3 "$2" | grep -q BAD; then
    echo "warning: $2: invalid instrumentation profile data (file header is corrupt)" >&2
    exit 1
  fi
  exit 0
fi
if [ "$1" = merge ]; then
  shift
  while [ $# -gt 0 ]; do
    if [ "$1" = -f ]; then cp "$2" "$MERGE_LOG"; fi
    shift
  done
  exit 0
fi
exit 2
"""


class MergeForwarderTests(unittest.TestCase):
    def run_merge(self, contents, floor=None):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            fake = root / "llvm-profdata"
            fake.write_text(FAKE_PROFDATA)
            fake.chmod(fake.stat().st_mode | stat.S_IXUSR)
            profiles = []
            for index, content in enumerate(contents):
                path = root / f"crates-{index}-1_0.profraw"
                path.write_bytes(content)
                profiles.append(path)
            input_list = root / "inputs.txt"
            input_list.write_text("".join(f"{path}\n" for path in profiles))
            diagnostics = root / "diagnostics"
            diagnostics.mkdir()
            env = dict(
                os.environ,
                COVERAGE_PROFILE_DIAGNOSTICS=str(diagnostics),
                COVERAGE_PROFILE_DIAGNOSTICS_MODE="forward",
                COVERAGE_REAL_PROFDATA=str(fake),
                MERGE_LOG=str(root / "merged-inputs.txt"),
            )
            if floor is not None:
                env["COVERAGE_PROFILE_DROP_FLOOR"] = str(floor)
            result = subprocess.run(
                [sys.executable, str(HELPER), "merge", "-sparse", "-f", str(input_list),
                 "-o", str(root / "out.profdata")],
                env=env, capture_output=True, text=True, check=False,
            )
            merged = root / "merged-inputs.txt"
            merged_names = (
                [Path(line).name for line in merged.read_text().splitlines()]
                if merged.exists() else None
            )
            dropped_path = diagnostics / "dropped.json"
            dropped = json.loads(dropped_path.read_text()) if dropped_path.exists() else None
            return result, merged_names, dropped

    def test_readable_profiles_merge_unchanged(self):
        result, merged, dropped = self.run_merge([b"OK1", b"OK2"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(merged, ["crates-0-1_0.profraw", "crates-1-1_0.profraw"])
        self.assertEqual(dropped["dropped"], [])
        self.assertNotIn("::warning", result.stderr)

    def test_one_unreadable_profile_is_excluded_and_announced(self):
        result, merged, dropped = self.run_merge([b"OK1", b"BAD", b"OK3"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(merged, ["crates-0-1_0.profraw", "crates-2-1_0.profraw"])
        self.assertEqual([item["path"].split("/")[-1] for item in dropped["dropped"]],
                         ["crates-1-1_0.profraw"])
        self.assertIn("file header is corrupt", dropped["dropped"][0]["reason"])
        self.assertIn("::warning title=Unreadable coverage profile excluded::crates-1-1_0.profraw",
                      result.stderr)
        self.assertEqual(dropped["floor"], 1)

    def test_more_unreadable_profiles_than_the_floor_refuse_the_merge(self):
        result, merged, dropped = self.run_merge([b"BAD", b"OK2", b"BAD"])
        self.assertEqual(result.returncode, 1)
        self.assertIsNone(merged, "the merge must not run past the floor")
        self.assertEqual(len(dropped["dropped"]), 2)
        self.assertIn("::error title=Too many unreadable coverage profiles::2 of 3", result.stderr)

    def test_floor_is_configurable(self):
        result, merged, dropped = self.run_merge([b"BAD", b"OK2", b"BAD"], floor=2)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(merged, ["crates-1-1_0.profraw"])
        self.assertEqual(dropped["floor"], 2)

    def test_all_unreadable_refuses_even_under_the_floor(self):
        result, merged, dropped = self.run_merge([b"BAD"], floor=5)
        self.assertEqual(result.returncode, 1)
        self.assertIsNone(merged)
        self.assertEqual(dropped["kept"], 0)
        self.assertIn("::error title=No readable coverage profile::", result.stderr)


if __name__ == "__main__":
    unittest.main()

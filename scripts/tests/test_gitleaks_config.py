#!/usr/bin/env python3
"""Exercise Gitleaks against disposable fixtures, never the project checkout.

Run: python3 -m unittest scripts.tests.test_gitleaks_config -v
Requires the installed gitleaks executable; CI runs this after its pinned install.
"""

from __future__ import annotations

import json
import os
import pathlib
import shutil
import subprocess
import tempfile
import unittest

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
CONFIG = REPO_ROOT / ".gitleaks.toml"
CACHE_PATH = "crates/khive-pack-git/src/cache.rs"
CACHE_VALUES = ("abcdef0123456789", "fedcba9876543210")
GATE_PATH = "crates/khive-runtime/src/secret_gate.rs"
GATE_VALUES = ("a3f5c2e9d1b8047e63a1f4c2d5b6e8f1a9c3d2e4", "Xk9mZ2vQpLrT8nJwYuA/HfBsDcGiONvMabcdefgh")
GITLEAKS = shutil.which("gitleaks")


@unittest.skipUnless(GITLEAKS, "install gitleaks to run scanner fixture checks")
class GitleaksConfigTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="gitleaks-config-test-")
        self.addCleanup(self.tmp.cleanup)
        self.root = pathlib.Path(self.tmp.name)
        self.source = self.root / "source"
        self.source.mkdir()
        self.config = self.root / "config.toml"
        self.config.write_text(CONFIG.read_text())
        self.report = self.root / "report.json"
        self.ignore = self.root / "empty-ignore"
        self.ignore.write_text("")
        # Do not inherit another repository, worktree, index, identity or config.
        self.env = {
            key: value for key, value in os.environ.items()
            if not key.startswith(("GIT_", "GITLEAKS_"))
        }
        self.env.update({
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_TERMINAL_PROMPT": "0",
            "GIT_AUTHOR_NAME": "Scanner Fixture",
            "GIT_AUTHOR_EMAIL": "scanner-fixture@example.invalid",
            "GIT_COMMITTER_NAME": "Scanner Fixture",
            "GIT_COMMITTER_EMAIL": "scanner-fixture@example.invalid",
        })

    def write_values(self, path=CACHE_PATH, values=CACHE_VALUES, padding=0):
        target = self.source / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(
            "// padding\n" * padding
            + "\n".join(f'let key_{i} = "{value}";' for i, value in enumerate(values))
            + "\n"
        )
        return target

    def scan(self, *, expected_exit, history=None, autodiscover=False):
        self.report.unlink(missing_ok=True)
        command = [
            GITLEAKS, "detect", "--source", ".", "--redact", "--no-banner",
            "--log-level", "error", "--gitleaks-ignore-path", str(self.ignore),
            "--report-format", "json", "--report-path", str(self.report),
        ]
        if not autodiscover:
            command += ["--config", str(self.config)]
        if history is None:
            command.append("--no-git")
        else:
            command.append(f"--log-opts={history}")
        result = subprocess.run(
            command, cwd=self.source, env=self.env, capture_output=True,
            text=True, timeout=20, check=False,
        )
        self.assertEqual(result.returncode, expected_exit, result.stderr)
        self.assertTrue(self.report.exists(), "scanner did not produce its JSON report")
        return json.loads(self.report.read_text())

    def git(self, *args):
        result = subprocess.run(
            ["git", *args], cwd=self.source, env=self.env, capture_output=True,
            text=True, timeout=10, check=True,
        )
        return result.stdout.strip()

    def commit_fixture(self, message):
        self.git("add", "--", CACHE_PATH)
        self.git("commit", "--quiet", "-m", message)
        return self.git("rev-parse", "HEAD")

    def test_defaults_detect_both_values_without_the_exemption(self):
        self.config.write_text("[extend]\nuseDefault = true\n")
        self.write_values()
        findings = self.scan(expected_exit=1)
        self.assertEqual(
            {(row["RuleID"], row["File"], row["StartLine"]) for row in findings},
            {("generic-api-key", CACHE_PATH, 1), ("generic-api-key", CACHE_PATH, 2)},
        )

    def test_ci_autodiscovery_accepts_exact_values_at_the_exact_path(self):
        (self.source / ".gitleaks.toml").write_text(CONFIG.read_text())
        self.write_values()
        self.assertEqual(self.scan(expected_exit=0, autodiscover=True), [])

    def test_exemption_survives_line_moves_and_new_commits_without_fingerprints(self):
        self.git("init", "--quiet")
        self.assertEqual(self.git("remote"), "")
        hooks = self.root / "empty-hooks"
        hooks.mkdir()
        self.git("config", "core.hooksPath", str(hooks))
        self.git("config", "commit.gpgsign", "false")
        self.write_values()
        first = self.commit_fixture("original fixtures")
        self.write_values(values=())
        removed = self.commit_fixture("remove fixtures before reintroduction")
        self.write_values(padding=17)
        moved = self.commit_fixture("reintroduce the same values at new lines")
        self.assertNotEqual(first, moved)

        # Positive control: the same values really are re-attributed to both
        # commits by the scanner, rather than ignored because no added lines ran.
        self.config.write_text("[extend]\nuseDefault = true\n")
        findings = self.scan(expected_exit=1, history="--all")
        self.assertEqual({row["Commit"] for row in findings}, {first, moved})
        moved_findings = self.scan(expected_exit=1, history=f"{removed}..{moved}")
        self.assertEqual({row["StartLine"] for row in moved_findings}, {18, 19})
        self.assertEqual({row["Commit"] for row in moved_findings}, {moved})

        self.config.write_text(CONFIG.read_text())
        self.assertEqual(self.scan(expected_exit=0, history="--all"), [])
        self.assertEqual(self.scan(expected_exit=0, history=f"{removed}..{moved}"), [])
        self.assertEqual(self.scan(expected_exit=0), [])

    def test_other_paths_and_path_prefixes_or_suffixes_are_not_exempt(self):
        paths = (
            "crates/khive-pack-git/src/other.rs",
            "prefix/" + CACHE_PATH,
            CACHE_PATH + ".bak",
        )
        for path in paths:
            self.write_values(path)
        self.write_values()  # the exempt site coexists with the positive controls
        findings = self.scan(expected_exit=1)
        self.assertEqual({row["File"] for row in findings}, set(paths))
        self.assertEqual(len(findings), 2 * len(paths))

    def test_unrelated_values_and_superstrings_at_the_same_site_are_not_exempt(self):
        other = "".join(reversed("72qQ93wW64eE85rR"))
        self.write_values(values=(other, "q7" + CACHE_VALUES[0], CACHE_VALUES[1] + "R8"))
        findings = self.scan(expected_exit=1)
        self.assertEqual({row["StartLine"] for row in findings}, {1, 2, 3})
        self.assertEqual({row["RuleID"] for row in findings}, {"generic-api-key"})

    def test_same_values_at_same_site_still_match_a_different_rule(self):
        # This fixture-only rule recognizes the very same bytes: a global
        # value/path exemption would wrongly suppress it along with the generic rule.
        self.config.write_text(CONFIG.read_text() + "\n" + "\n".join([
            "[[rules]]", 'id = "fixture-other-rule"',
            "regex = '''(" + "|".join(CACHE_VALUES) + ")'''",
        ]) + "\n")
        self.write_values()
        findings = self.scan(expected_exit=1)
        self.assertEqual(len(findings), 2)
        self.assertEqual({row["RuleID"] for row in findings}, {"fixture-other-rule"})

    def test_other_default_rules_remain_enabled(self):
        # Synthetic provider-shaped data, assembled only in the disposable input.
        provider_value = "ghp_" + "a7B4c9D2e5F8" * 3
        self.write_values(values=(provider_value,))
        findings = self.scan(expected_exit=1)
        self.assertIn("github-pat", {row["RuleID"] for row in findings})


    def test_gate_constants_are_exempt_at_the_gate_path(self):
        self.write_values(GATE_PATH, GATE_VALUES)
        self.assertEqual(self.scan(expected_exit=0), [])

    def test_gate_constants_are_not_exempt_at_other_paths(self):
        paths = (
            "crates/khive-runtime/src/other.rs",
            "prefix/" + GATE_PATH,
            GATE_PATH + ".bak",
        )
        for path in paths:
            self.write_values(path, GATE_VALUES)
        self.write_values(GATE_PATH, GATE_VALUES)  # the exempt site coexists
        findings = self.scan(expected_exit=1)
        self.assertEqual({row["File"] for row in findings}, set(paths))
        self.assertEqual(len(findings), len(GATE_VALUES) * len(paths))

    def test_neighbouring_values_at_the_gate_path_are_not_exempt(self):
        # Superstrings and a sibling fixture shape: the exemption is two constants,
        # not the file, so a real credential added here still has to be reported.
        neighbours = (
            GATE_VALUES[0] + "ff",
            "ff" + GATE_VALUES[1],
            "".join(reversed(GATE_VALUES[0])),
        )
        self.write_values(GATE_PATH, neighbours)
        findings = self.scan(expected_exit=1)
        self.assertEqual({row["StartLine"] for row in findings}, {1, 2, 3})
        self.assertEqual({row["File"] for row in findings}, {GATE_PATH})

    def test_gate_exemption_survives_line_moves_and_new_commits(self):
        # The defect this exemption replaces: a fingerprint pins commit and line,
        # so editing the file anywhere above the constant reintroduced the finding.
        self.git("init", "--quiet")
        hooks = self.root / "empty-hooks"
        hooks.mkdir()
        self.git("config", "core.hooksPath", str(hooks))
        self.git("config", "commit.gpgsign", "false")
        self.write_values(GATE_PATH, GATE_VALUES)
        self.git("add", "--", GATE_PATH)
        self.git("commit", "--quiet", "-m", "original fixtures")
        first = self.git("rev-parse", "HEAD")
        self.write_values(GATE_PATH, ())
        self.git("add", "--", GATE_PATH)
        self.git("commit", "--quiet", "-m", "remove fixtures before reintroduction")
        self.write_values(GATE_PATH, GATE_VALUES, padding=31)
        self.git("add", "--", GATE_PATH)
        self.git("commit", "--quiet", "-m", "reintroduce the same values at new lines")
        moved = self.git("rev-parse", "HEAD")
        self.assertNotEqual(first, moved)

        # Positive control: without the exemption the scanner really does report
        # the values under both commits, so the pass below is not an empty scan.
        self.config.write_text("[extend]\nuseDefault = true\n")
        findings = self.scan(expected_exit=1, history="--all")
        self.assertEqual({row["Commit"] for row in findings}, {first, moved})

        self.config.write_text(CONFIG.read_text())
        self.assertEqual(self.scan(expected_exit=0, history="--all"), [])

    def test_every_exempt_gate_constant_still_appears_in_the_file_it_exempts(self):
        # An exemption outlives the fixture it was written for. This fails when a
        # constant is renamed or dropped, instead of leaving a dead entry that
        # quietly widens what the scanner is told to skip.
        source = (REPO_ROOT / GATE_PATH).read_text()
        for value in GATE_VALUES:
            self.assertIn(
                value, source,
                f"exempt constant no longer present in {GATE_PATH}; remove the exemption",
            )

if __name__ == "__main__":
    unittest.main()

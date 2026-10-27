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
CACHE_HISTORY_PATH = "crates/khive-pack-git/src/cache.rs"
CACHE_PATH = "crates/khive-pack-git/src/cache_tests.rs"
CACHE_PATHS = (CACHE_HISTORY_PATH, CACHE_PATH)
CACHE_VALUES = ("abcdef0123456789", "fedcba9876543210")
GATE_HISTORY_PATH = "crates/khive-runtime/src/secret_gate.rs"
GATE_PATH = "crates/khive-runtime/src/secret_gate_tests.rs"
GATE_PATHS = (GATE_HISTORY_PATH, GATE_PATH)
GATE_VALUES = ("a3f5c2e9d1b8047e63a1f4c2d5b6e8f1a9c3d2e4", "Xk9mZ2vQpLrT8nJwYuA/HfBsDcGiONvMabcdefgh")
# A published specification value that stays in history after its fixture left the tree.
HPKE_VECTOR_PATH = "crates/khive-channel-node/tests/fixtures/rfc9180-auth-x25519-chacha.json"
HPKE_VECTOR_VALUES = ("d2d67828c8bc9fa661cf15a31b3ebf1febe0cafef7abfaaca580aaf6d471e3eb",)
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

    def commit_fixture(self, message, path=CACHE_PATH):
        self.git("add", "--", path)
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

    def test_ci_autodiscovery_accepts_exact_values_at_both_exact_paths(self):
        (self.source / ".gitleaks.toml").write_text(CONFIG.read_text())
        for path in CACHE_PATHS:
            self.write_values(path)
        self.assertEqual(self.scan(expected_exit=0, autodiscover=True), [])

    def test_exemption_survives_file_moves_and_new_commits_without_fingerprints(self):
        self.git("init", "--quiet")
        self.assertEqual(self.git("remote"), "")
        hooks = self.root / "empty-hooks"
        hooks.mkdir()
        self.git("config", "core.hooksPath", str(hooks))
        self.git("config", "commit.gpgsign", "false")
        self.write_values(CACHE_HISTORY_PATH)
        first = self.commit_fixture("original fixtures", CACHE_HISTORY_PATH)
        self.write_values(CACHE_HISTORY_PATH, values=())
        removed = self.commit_fixture("remove fixtures before extraction", CACHE_HISTORY_PATH)
        self.write_values(padding=17)
        moved = self.commit_fixture("extract the same values to the test module")
        self.assertNotEqual(first, moved)

        # Positive control: both paths and commits really contain reportable
        # values, so a passing history scan cannot be an empty scan.
        self.config.write_text("[extend]\nuseDefault = true\n")
        findings = self.scan(expected_exit=1, history="--all")
        self.assertEqual({row["Commit"] for row in findings}, {first, moved})
        self.assertEqual({row["File"] for row in findings}, set(CACHE_PATHS))
        moved_findings = self.scan(expected_exit=1, history=f"{removed}..{moved}")
        self.assertEqual({row["StartLine"] for row in moved_findings}, {18, 19})
        self.assertEqual({row["Commit"] for row in moved_findings}, {moved})
        self.assertEqual({row["File"] for row in moved_findings}, {CACHE_PATH})

        self.config.write_text(CONFIG.read_text())
        self.assertEqual(self.scan(expected_exit=0, history="--all"), [])
        self.assertEqual(self.scan(expected_exit=0, history=f"{removed}..{moved}"), [])
        self.assertEqual(self.scan(expected_exit=0), [])

        # Each exact path is necessary: removing it exposes only that history.
        for path, commit in [(CACHE_HISTORY_PATH, first), (CACHE_PATH, moved)]:
            entry = "  '''^" + path.replace(".", r"\.") + "$''',\n"
            config = CONFIG.read_text()
            self.assertEqual(config.count(entry), 1)
            self.config.write_text(config.replace(entry, "", 1))
            findings = self.scan(expected_exit=1, history="--all")
            self.assertEqual({row["File"] for row in findings}, {path})
            self.assertEqual({row["Commit"] for row in findings}, {commit})

    def test_other_paths_and_path_prefixes_or_suffixes_are_not_exempt(self):
        paths = (
            "crates/khive-pack-git/src/other.rs",
            *("prefix/" + path for path in CACHE_PATHS),
            *(path + ".bak" for path in CACHE_PATHS),
        )
        for path in paths:
            self.write_values(path)
        for path in CACHE_PATHS:
            self.write_values(path)  # both exempt sites coexist with the controls
        findings = self.scan(expected_exit=1)
        self.assertEqual({row["File"] for row in findings}, set(paths))
        self.assertEqual(len(findings), 2 * len(paths))

    def test_unrelated_values_and_superstrings_at_both_sites_are_not_exempt(self):
        other = "".join(reversed("72qQ93wW64eE85rR"))
        for path in CACHE_PATHS:
            self.write_values(path, values=(other, "q7" + CACHE_VALUES[0], CACHE_VALUES[1] + "R8"))
        findings = self.scan(expected_exit=1)
        self.assertEqual(
            {(row["File"], row["StartLine"]) for row in findings},
            {(path, line) for path in CACHE_PATHS for line in (1, 2, 3)},
        )
        self.assertEqual({row["RuleID"] for row in findings}, {"generic-api-key"})

    def test_same_values_at_same_site_still_match_a_different_rule(self):
        # This fixture-only rule recognizes the very same bytes: a global
        # value/path exemption would wrongly suppress it along with the generic rule.
        self.config.write_text(CONFIG.read_text() + "\n" + "\n".join([
            "[[rules]]", 'id = "fixture-other-rule"',
            "regex = '''(" + "|".join(CACHE_VALUES) + ")'''",
        ]) + "\n")
        for path in CACHE_PATHS:
            self.write_values(path)
        findings = self.scan(expected_exit=1)
        self.assertEqual(len(findings), 2 * len(CACHE_PATHS))
        self.assertEqual({row["File"] for row in findings}, set(CACHE_PATHS))
        self.assertEqual({row["RuleID"] for row in findings}, {"fixture-other-rule"})

    def test_every_exempt_cache_constant_still_appears_in_the_current_fixture(self):
        source = (REPO_ROOT / CACHE_PATH).read_text()
        for value in CACHE_VALUES:
            self.assertIn(
                value, source,
                f"exempt constant no longer present in {CACHE_PATH}; remove the exemption",
            )

    def test_other_default_rules_remain_enabled(self):
        # Synthetic provider-shaped data, assembled only in the disposable input.
        provider_value = "ghp_" + "a7B4c9D2e5F8" * 3
        self.write_values(values=(provider_value,))
        findings = self.scan(expected_exit=1)
        self.assertIn("github-pat", {row["RuleID"] for row in findings})


    def test_gate_constants_are_exempt_at_the_gate_path(self):
        for path in GATE_PATHS:
            self.write_values(path, GATE_VALUES)
        self.assertEqual(self.scan(expected_exit=0), [])

    def baseline_findings(self):
        """What the scanner reports with no exemption, for the fixture already written.

        Read rather than assumed: which of these synthetic values the default rules
        flag differs between scanner versions, so an arm that hard-codes a count is
        asserting the scanner's breadth instead of the exemption's reach. Every
        comparison below is against this same-run baseline.
        """
        keep = self.config.read_text()
        self.config.write_text("[extend]\nuseDefault = true\n")
        try:
            return self.scan(expected_exit=1)
        finally:
            self.config.write_text(keep)

    def test_gate_constants_are_not_exempt_at_other_paths(self):
        paths = (
            "crates/khive-runtime/src/other.rs",
            "crates/khive-runtime/src/secret_gate/corpus_replay_tests.rs",
            *("prefix/" + path for path in GATE_PATHS),
            *(path + ".bak" for path in GATE_PATHS),
        )
        for path in paths:
            self.write_values(path, GATE_VALUES)
        for path in GATE_PATHS:
            self.write_values(path, GATE_VALUES)  # both exact sites coexist
        baseline = self.baseline_findings()
        self.assertEqual(
            {row["File"] for row in baseline}, set(paths) | set(GATE_PATHS),
            "control: without the exemption every path must report, or this arm proves nothing",
        )
        findings = self.scan(expected_exit=1)
        self.assertEqual({row["File"] for row in findings}, set(paths))
        self.assertEqual(
            {row["Fingerprint"] for row in findings},
            {row["Fingerprint"] for row in baseline if row["File"] not in GATE_PATHS},
            "the exemption must remove the exempt path's findings and nothing else",
        )

    def test_neighbouring_values_at_the_gate_path_are_not_exempt(self):
        # Superstrings and a sibling fixture shape: the exemption is two constants,
        # not the file, so a real credential added here still has to be reported.
        neighbours = (
            GATE_VALUES[0] + "ff",
            "ff" + GATE_VALUES[1],
            "".join(reversed(GATE_VALUES[0])),
        )
        for path in GATE_PATHS:
            self.write_values(path, neighbours)
        baseline = self.baseline_findings()
        self.assertTrue(baseline, "control: the neighbours must be reportable to begin with")
        findings = self.scan(expected_exit=1)
        self.assertEqual({row["File"] for row in findings}, set(GATE_PATHS))
        self.assertEqual(
            {row["Fingerprint"] for row in findings},
            {row["Fingerprint"] for row in baseline},
            "a value that merely resembles an exempt constant must still be reported",
        )

    def test_gate_exemption_survives_line_moves_and_new_commits(self):
        # The defect this exemption replaces: a fingerprint pins commit and line,
        # so editing the file anywhere above the constant reintroduced the finding.
        self.git("init", "--quiet")
        hooks = self.root / "empty-hooks"
        hooks.mkdir()
        self.git("config", "core.hooksPath", str(hooks))
        self.git("config", "commit.gpgsign", "false")
        self.write_values(GATE_HISTORY_PATH, GATE_VALUES)
        self.git("add", "--", GATE_HISTORY_PATH)
        self.git("commit", "--quiet", "-m", "original fixtures")
        first = self.git("rev-parse", "HEAD")
        self.write_values(GATE_HISTORY_PATH, ())
        self.git("add", "--", GATE_HISTORY_PATH)
        self.git("commit", "--quiet", "-m", "remove fixtures before reintroduction")
        self.write_values(GATE_PATH, GATE_VALUES, padding=31)
        self.git("add", "--", GATE_PATH)
        self.git("commit", "--quiet", "-m", "extract the same values to the test module")
        moved = self.git("rev-parse", "HEAD")
        self.assertNotEqual(first, moved)

        # Positive control: without the exemption the scanner really does report
        # the values under both commits, so the pass below is not an empty scan.
        self.config.write_text("[extend]\nuseDefault = true\n")
        findings = self.scan(expected_exit=1, history="--all")
        self.assertEqual({row["Commit"] for row in findings}, {first, moved})
        self.assertEqual({row["File"] for row in findings}, set(GATE_PATHS))

        self.config.write_text(CONFIG.read_text())
        self.assertEqual(self.scan(expected_exit=0, history="--all"), [])
        self.assertEqual(self.scan(expected_exit=0), [])

        # Each exact path is necessary: removing it exposes only that history.
        for path, commit in [(GATE_HISTORY_PATH, first), (GATE_PATH, moved)]:
            entry = "  '''^" + path.replace(".", r"\.") + "$''',\n"
            config = CONFIG.read_text()
            self.assertEqual(config.count(entry), 1)
            self.config.write_text(config.replace(entry, "", 1))
            findings = self.scan(expected_exit=1, history="--all")
            self.assertEqual({row["File"] for row in findings}, {path})
            self.assertEqual({row["Commit"] for row in findings}, {commit})

    def test_gate_values_at_both_paths_still_match_a_different_rule(self):
        self.config.write_text(CONFIG.read_text() + "\n" + "\n".join([
            "[[rules]]", 'id = "fixture-other-gate-rule"',
            "regex = '''(" + "|".join(GATE_VALUES) + ")'''",
        ]) + "\n")
        for path in GATE_PATHS:
            self.write_values(path, GATE_VALUES)
        findings = self.scan(expected_exit=1)
        self.assertEqual(len(findings), len(GATE_VALUES) * len(GATE_PATHS))
        self.assertEqual({row["File"] for row in findings}, set(GATE_PATHS))
        self.assertEqual({row["RuleID"] for row in findings}, {"fixture-other-gate-rule"})

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

    def without_hpke_vector_entry(self):
        """The checked-in config with the HPKE vector's allowlist entry, and only that one, removed."""
        blocks = CONFIG.read_text().split("[[rules.allowlists]]")
        kept = [block for block in blocks if HPKE_VECTOR_PATH.replace(".", r"\.") not in block]
        self.assertEqual(len(blocks) - len(kept), 1, "expected exactly one entry naming the vector's path")
        return "[[rules.allowlists]]".join(kept)

    def test_hpke_vector_is_exempt_at_its_fixture_path(self):
        self.write_values(HPKE_VECTOR_PATH, HPKE_VECTOR_VALUES)
        baseline = self.baseline_findings()
        self.assertEqual({row["File"] for row in baseline}, {HPKE_VECTOR_PATH},
                         "control: without the exemption the vector must report")
        self.assertEqual(self.scan(expected_exit=0), [])

    def test_hpke_vector_is_not_exempt_at_other_paths(self):
        paths = (
            "crates/khive-channel-node/tests/fixtures/rfc9180-base-x25519-chacha.json",
            "docs/adr/ADR-105-cross-node-comm-transport.md",
            "prefix/" + HPKE_VECTOR_PATH,
            HPKE_VECTOR_PATH + ".bak",
        )
        for path in (*paths, HPKE_VECTOR_PATH):
            self.write_values(path, HPKE_VECTOR_VALUES)
        baseline = self.baseline_findings()
        self.assertEqual(
            {row["File"] for row in baseline}, {*paths, HPKE_VECTOR_PATH},
            "control: without the exemption every path must report, or this arm proves nothing",
        )
        findings = self.scan(expected_exit=1)
        self.assertEqual({row["File"] for row in findings}, set(paths))
        self.assertEqual(
            {row["Fingerprint"] for row in findings},
            {row["Fingerprint"] for row in baseline if row["File"] != HPKE_VECTOR_PATH},
            "the exemption must remove the fixture path's finding and nothing else",
        )

    def test_neighbouring_values_at_the_hpke_vector_path_are_not_exempt(self):
        value = HPKE_VECTOR_VALUES[0]
        self.write_values(HPKE_VECTOR_PATH, (value + "ff", "ff" + value, "".join(reversed(value))))
        baseline = self.baseline_findings()
        self.assertEqual(len(baseline), 3, "control: all three neighbours must be reportable")
        findings = self.scan(expected_exit=1)
        self.assertEqual(
            {row["Fingerprint"] for row in findings},
            {row["Fingerprint"] for row in baseline},
            "a value that merely resembles the vector must still be reported",
        )

    def test_hpke_exemption_covers_history_after_the_fixture_left_the_tree(self):
        self.git("init", "--quiet")
        hooks = self.root / "empty-hooks"
        hooks.mkdir()
        self.git("config", "core.hooksPath", str(hooks))
        self.git("config", "commit.gpgsign", "false")
        self.write_values(HPKE_VECTOR_PATH, HPKE_VECTOR_VALUES, padding=15)
        added = self.commit_fixture("add the published vector", HPKE_VECTOR_PATH)
        (self.source / HPKE_VECTOR_PATH).unlink()
        self.git("rm", "--quiet", "--cached", "--", HPKE_VECTOR_PATH)
        self.git("commit", "--quiet", "-m", "remove the fixture from the tree")

        self.assertEqual(self.scan(expected_exit=0), [], "the tree no longer holds the fixture")
        self.assertEqual(self.scan(expected_exit=0, history="--all"), [])

        # Control: dropping this one entry brings back exactly the history finding.
        self.config.write_text(self.without_hpke_vector_entry())
        findings = self.scan(expected_exit=1, history="--all")
        self.assertEqual(
            {(row["File"], row["Commit"], row["StartLine"]) for row in findings},
            {(HPKE_VECTOR_PATH, added, 16)},
        )

    def test_hpke_vector_at_its_path_still_matches_a_different_rule(self):
        self.config.write_text(CONFIG.read_text() + "\n" + "\n".join([
            "[[rules]]", 'id = "fixture-other-vector-rule"',
            "regex = '''(" + "|".join(HPKE_VECTOR_VALUES) + ")'''",
        ]) + "\n")
        self.write_values(HPKE_VECTOR_PATH, HPKE_VECTOR_VALUES)
        findings = self.scan(expected_exit=1)
        self.assertEqual({row["RuleID"] for row in findings}, {"fixture-other-vector-rule"})
        self.assertEqual({row["File"] for row in findings}, {HPKE_VECTOR_PATH})

    def test_the_exempt_hpke_vector_is_still_in_the_fixture_history(self):
        # The fixture left the tree, so the check that keeps this entry from going stale reads history.
        def repo_git(*args):
            return subprocess.run(["git", "-C", str(REPO_ROOT), *args], capture_output=True,
                                  text=True, timeout=60, check=False)
        shallow = repo_git("rev-parse", "--is-shallow-repository")
        if shallow.returncode != 0 or shallow.stdout.strip() != "false":
            self.skipTest("needs a full-history checkout")
        commits = repo_git("log", "--all", "--format=%H", "--", HPKE_VECTOR_PATH).stdout.split()
        self.assertTrue(commits, f"no commit touches {HPKE_VECTOR_PATH}; remove the exemption")
        contents = [repo_git("show", f"{commit}:{HPKE_VECTOR_PATH}").stdout for commit in commits]
        for value in HPKE_VECTOR_VALUES:
            self.assertTrue(
                any(value in text for text in contents),
                f"exempt value not found in the history of {HPKE_VECTOR_PATH}; remove the exemption",
            )

if __name__ == "__main__":
    unittest.main()

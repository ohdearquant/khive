#!/usr/bin/env python3
"""Fixture tests for scripts/check-adr-labels.py. Run with:
    python3 scripts/test_check_adr_labels.py
"""
import importlib.util
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT_PATH = Path(__file__).resolve().parent / "check-adr-labels.py"
_spec = importlib.util.spec_from_file_location("check_adr_labels", SCRIPT_PATH)
check_adr_labels = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(check_adr_labels)


def write(path, content):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


class ADRLabelCheckTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name)
        self.adr_dir = self.root / "docs" / "adr"
        self.crates_dir = self.root / "crates"
        self.adr_dir.mkdir(parents=True)
        self.crates_dir.mkdir(parents=True)
        self._orig = (
            check_adr_labels.REPO_ROOT,
            check_adr_labels.ADR_DIR,
            check_adr_labels.CRATES_DIR,
        )
        check_adr_labels.REPO_ROOT = self.root
        check_adr_labels.ADR_DIR = self.adr_dir
        check_adr_labels.CRATES_DIR = self.crates_dir

    def tearDown(self):
        (
            check_adr_labels.REPO_ROOT,
            check_adr_labels.ADR_DIR,
            check_adr_labels.CRATES_DIR,
        ) = self._orig
        self._tmp.cleanup()

    def run_check(self):
        registry, registry_errors = check_adr_labels.build_registry()
        design_errors = check_adr_labels.check_design_docs(registry)
        return registry, sorted(registry_errors) + sorted(design_errors)

    def test_exact_canonical_title_passes(self):
        write(self.adr_dir / "ADR-001-foo.md", "# ADR-001: Foo Bar\n\ncontent\n")
        write(
            self.crates_dir / "khive-x/docs/design.md",
            "# X\n\n### ADR-001: Foo Bar\n\nbody\n",
        )
        _, errors = self.run_check()
        self.assertEqual(errors, [])

    def test_wrong_subject_label_fails_with_location_and_replacement(self):
        write(self.adr_dir / "ADR-004-substrate.md", "# ADR-004: Substrate Observables\n")
        write(
            self.crates_dir / "khive-y/docs/design.md",
            "\n\n### ADR-004: NoteKindSpec lifecycle declaration\n",
        )
        _, errors = self.run_check()
        self.assertEqual(len(errors), 1)
        self.assertIn("khive-y/docs/design.md:3", errors[0])
        self.assertIn("Substrate Observables", errors[0])
        self.assertIn(
            "replace with 'ADR-004: Substrate Observables - NoteKindSpec lifecycle declaration'",
            errors[0],
        )

    def test_normalized_dash_with_local_qualifier_passes(self):
        # Uses a unicode escape, not a literal character, so this source file
        # itself stays free of non-ASCII dash bytes.
        em_dash = "—"
        write(self.adr_dir / "ADR-002-edge.md", "# ADR-002: Closed Edge Ontology\n")
        write(
            self.crates_dir / "khive-z/docs/design.md",
            f"### ADR-002: Closed Edge Ontology {em_dash} 17 relations\n",
        )
        _, errors = self.run_check()
        self.assertEqual(errors, [])

    def test_unknown_adr_id_fails(self):
        write(self.adr_dir / "ADR-001-foo.md", "# ADR-001: Foo Bar\n")
        write(
            self.crates_dir / "khive-x/docs/design.md",
            "### ADR-999: Nonexistent\n",
        )
        _, errors = self.run_check()
        self.assertEqual(len(errors), 1)
        self.assertIn("unknown ADR-999", errors[0])

    def test_adr_007_style_rev_resolves_through_current_h1(self):
        write(
            self.adr_dir / "ADR-007-namespace.md",
            "# ADR-007 Rev 7: Namespace Contract\n\nbody\n\n---\n\n"
            "# ADR-007 Rev 6: Old Namespace Contract\n",
        )
        write(
            self.crates_dir / "khive-g/docs/design.md",
            "### ADR-007: Namespace Contract\n",
        )
        registry, errors = self.run_check()
        self.assertEqual(errors, [])
        self.assertEqual(registry["007"][1], "Namespace Contract")

    def test_prose_and_multi_id_headings_are_ignored(self):
        write(self.adr_dir / "ADR-024-fold.md", "# ADR-024: Fold Cognitive Primitives\n")
        write(self.adr_dir / "ADR-025-verb.md", "# ADR-025: Verb Surface\n")
        write(
            self.crates_dir / "khive-p/docs/design.md",
            "\n".join(
                [
                    "See ADR-024 section \"extensions\" for details.",
                    '### ADR-024 §"Bayesian extensions": Selector Budget Packing',
                    "### ADR-024 / ADR-025: Fold and Verb Interplay",
                    "",
                ]
            ),
        )
        _, errors = self.run_check()
        self.assertEqual(errors, [])

    def test_mutation_of_known_good_label_fails_then_passes_after_restoration(self):
        write(self.adr_dir / "ADR-001-foo.md", "# ADR-001: Foo Bar\n")
        design_path = self.crates_dir / "khive-x/docs/design.md"
        write(design_path, "### ADR-001: Foo Bar\n")
        _, errors = self.run_check()
        self.assertEqual(errors, [])

        write(design_path, "### ADR-001: Totally Different Subject\n")
        _, errors = self.run_check()
        self.assertEqual(len(errors), 1)

        write(design_path, "### ADR-001: Foo Bar\n")
        _, errors = self.run_check()
        self.assertEqual(errors, [])

    def test_amendment_companion_file_is_not_a_duplicate_error(self):
        write(self.adr_dir / "ADR-088-base.md", "# ADR-088: Git Lifecycle Pack\n")
        write(
            self.adr_dir / "ADR-088-amendment-1.md",
            "# ADR-088 Amendment 1: git.digest Verb\n",
        )
        registry, errors = self.run_check()
        self.assertEqual(errors, [])
        self.assertEqual(registry["088"][1], "Git Lifecycle Pack")

    def test_true_duplicate_unqualified_canonical_h1_fails(self):
        write(self.adr_dir / "ADR-001-a.md", "# ADR-001: Foo Bar\n")
        write(self.adr_dir / "ADR-001-b.md", "# ADR-001: Foo Bar Again\n")
        _, errors = self.run_check()
        self.assertEqual(len(errors), 1)
        self.assertIn("duplicate unqualified canonical H1", errors[0])

    def test_malformed_canonical_h1_fails(self):
        write(self.adr_dir / "ADR-001-bad.md", "Not a heading at all\n")
        _, errors = self.run_check()
        self.assertEqual(len(errors), 1)
        self.assertIn("malformed canonical H1", errors[0])


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3

import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

from documented_verb_counts import (
    RegistryCounts,
    _published_files,
    is_published_markdown,
    registry_counts,
    scan_document,
    scan_repository,
    validate_documented_counts,
)


PACK_COUNTS = {"kg": 20, "comm": 8, "workspace": 0}
EXTENDED_PACK_COUNTS = {**PACK_COUNTS, "git": 4, "blob": 3}


def verbs_result(pack_counts: dict[str, int] = PACK_COUNTS) -> dict:
    verbs = [
        {"verb": f"{pack}.{index}", "pack": pack}
        for pack, count in pack_counts.items()
        for index in range(count)
    ]
    return {"verbs": verbs, "total": len(verbs), "pack_counts": pack_counts}


class DocumentedVerbCountsTest(unittest.TestCase):
    def test_registry_counts_require_consistent_per_pack_summary(self) -> None:
        counts = registry_counts(verbs_result())
        self.assertEqual(counts.total_verbs, 28)
        self.assertEqual(counts.total_packs, 3)
        self.assertEqual(counts.pack_verbs["workspace"], 0)

        malformed = verbs_result()
        malformed["pack_counts"] = {**PACK_COUNTS, "comm": 7}
        with self.assertRaisesRegex(ValueError, "pack_counts"):
            registry_counts(malformed)

    def test_scanner_covers_requested_claim_forms(self) -> None:
        text = """A 28-verb runtime.
28 verbs across 3 packs.
verbs: 28
All 3 packs load by default.
The server config loads all three (`kg`, `comm`, `workspace`).
## `kg` pack — 20 verbs
| Pack | Verbs |
| --- | --- |
| **comm** | 8 |
| **workspace** | 0 |
"""
        claims = scan_document("README.md", text, PACK_COUNTS)
        forms = {claim.form for claim in claims}
        self.assertTrue({"hyphenated", "spaced", "inverted", "spelled", "per-pack-table"} <= forms)
        self.assertIn(("pack_verbs", "kg", 20), {(c.kind, c.pack, c.value) for c in claims})
        self.assertIn(("pack_verbs", "workspace", 0), {(c.kind, c.pack, c.value) for c in claims})

    def test_scanner_covers_status_cells_and_line_wrapped_pack_counts(self) -> None:
        claims = scan_document(
            "README.md",
            "| **28 verbs, 3 packs** | all load by default |\n"
            "A 28-verb runtime across 3\n"
            "packs, ready for use.\n",
            PACK_COUNTS,
        )
        values = [(claim.kind, claim.value) for claim in claims]
        self.assertEqual(values.count(("total_verbs", 28)), 2)
        self.assertEqual(values.count(("total_packs", 3)), 2)

    def test_living_multicolumn_pack_table_detects_stale_mutation(self) -> None:
        published = """28 verbs across 3 production packs.
| Pack          | Prefix   | Verbs | What it does |
| ------------- | -------- | ----- | ------------ |
| **kg**        | _(bare)_ | 20    | Graph        |
| **comm**      | `comm.`  | 8     | Messaging    |
| **workspace** | _(none)_ | 0     | Vocabulary   |
"""
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            readme = root / "README.md"
            readme.write_text(published, encoding="utf-8")
            self.assertEqual(validate_documented_counts(root, verbs_result()), [])

            readme.write_text(
                published.replace(
                    "| **comm**      | `comm.`  | 8",
                    "| **comm**      | `comm.`  | 7",
                ),
                encoding="utf-8",
            )
            errors = validate_documented_counts(root, verbs_result())

        self.assertEqual(len(errors), 1)
        self.assertIn("claims comm verbs=7, registry says 8", errors[0])

    def test_escaped_pipe_before_pack_columns_cannot_hide_stale_count(self) -> None:
        for outer in ("closed", "open", "repeated"):
            def frame(cells: str) -> str:
                return {
                    "closed": f"| {cells} |",
                    "open": f"| {cells}",
                    "repeated": f"|| {cells} ||",
                }[outer]

            with self.subTest(outer=outer), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                readme = root / "README.md"
                header = frame("Example | Pack | Verbs")
                separator = frame("--- | --- | ---")
                for count in (20, 99):
                    text = "\n".join((
                        "28 verbs across 3 packs.",
                        header,
                        separator,
                        frame(r"a\|b | kg | " + str(count)),
                        frame("plain | comm | 8"),
                        frame("plain | workspace | 0"),
                    )) + "\n"
                    readme.write_text(text, encoding="utf-8")
                    claims = scan_document("README.md", text, PACK_COUNTS)
                    table_claims = [claim for claim in claims if claim.form == "per-pack-table"]
                    self.assertEqual(
                        [(claim.pack, claim.value) for claim in table_claims],
                        [("kg", count), ("comm", 8), ("workspace", 0)],
                    )
                    errors = validate_documented_counts(root, verbs_result())
                    if count == 20:
                        self.assertEqual(errors, [])
                    else:
                        self.assertEqual(len(errors), 1)
                        self.assertIn("README.md:4:", errors[0])
                        self.assertIn("claims kg verbs=99, registry says 20", errors[0])

    def test_count_tables_keep_empty_cells_and_optional_closing_pipe(self) -> None:
        for suffix in ("", "|", "||"):
            with self.subTest(suffix=suffix):
                text = "| Example | Pack | Verbs " + suffix + "\n"
                text += "| --- | --- | --- " + suffix + "\n"
                text += "| | kg | 99 " + suffix + "\n"
                claims = scan_document("README.md", text, PACK_COUNTS)
                self.assertEqual(
                    [(claim.pack, claim.value) for claim in claims if claim.form == "per-pack-table"],
                    [("kg", 99)],
                )

    def test_living_pages_and_inverted_mutations_fail_validation(self) -> None:
        published = """28 verbs across 3 production packs.
| Pack | Verbs |
| ---- | ----- |
| kg | 20 |
| comm | 8 |
| workspace | 0 |
"""
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "README.md").write_text(published, encoding="utf-8")
            workflow = root / ".github/workflows/pages.yml"
            workflow.parent.mkdir(parents=True)
            workflow.write_text(
                "Full verb catalog for all 2 production packs.\n",
                encoding="utf-8",
            )
            errors = validate_documented_counts(root, verbs_result())
            self.assertEqual(len(errors), 1)
            self.assertIn("claims total packs=2, registry says 3", errors[0])

            workflow.write_text(
                "Full verb catalog for all 3 production packs.\n",
                encoding="utf-8",
            )
            pack_readme = root / "crates/khive-pack-comm/README.md"
            pack_readme.parent.mkdir(parents=True)
            pack_readme.write_text("Verbs: 7\n", encoding="utf-8")
            errors = validate_documented_counts(root, verbs_result())

        self.assertEqual(len(errors), 1)
        self.assertIn("claims comm verbs=7, registry says 8", errors[0])

    def test_living_modifier_forms_are_scanned(self) -> None:
        pages_claims = scan_document(
            ".github/workflows/pages.yml",
            'Full verb catalog for all 3 production packs: params and examples.\n',
            PACK_COUNTS,
        )
        self.assertIn(
            ("total_packs", 3),
            {(claim.kind, claim.value) for claim in pages_claims},
        )

        session_counts = {**PACK_COUNTS, "session": 4}
        session_claims = scan_document(
            "crates/khive-pack-session/README.md",
            "Session pack: registers the session note kind and four agent-facing verbs.\n",
            session_counts,
        )
        self.assertEqual(
            [(claim.kind, claim.pack, claim.value) for claim in session_claims],
            [("pack_verbs", "session", 4)],
        )

    def test_pack_path_wins_for_unqualified_inverted_claim(self) -> None:
        pack_claims = scan_document(
            "crates/khive-pack-comm/README.md",
            "Verbs: 8\nRuntime details follow.\n",
            PACK_COUNTS,
        )
        self.assertEqual(
            [(claim.kind, claim.pack, claim.value) for claim in pack_claims],
            [("pack_verbs", "comm", 8)],
        )

        aggregate_claims = scan_document(
            "crates/khive-pack-comm/README.md",
            "Verbs: 28 across 3 packs.\n",
            PACK_COUNTS,
        )
        self.assertIn(
            ("total_verbs", None, 28),
            {(claim.kind, claim.pack, claim.value) for claim in aggregate_claims},
        )

    def test_pack_path_supplies_named_window(self) -> None:
        claims = scan_document(
            "marketplace/khive/skills/comm/SKILL.md",
            "The surface is eight verbs, all public.\n",
            PACK_COUNTS,
        )
        self.assertEqual([(c.pack, c.value) for c in claims], [("comm", 8)])

        claims = scan_document(
            "crates/khive-pack-comm/src/lib.rs",
            "//! Adds the `message` note kind and eight verbs.\n",
            PACK_COUNTS,
        )
        self.assertEqual([(c.pack, c.value) for c in claims], [("comm", 8)])

        claims = scan_document(
            "crates/khive-pack-kg/README.md",
            "## Verbs\n\n20 handlers, registered under ADR-017.\n",
            PACK_COUNTS,
        )
        self.assertEqual([(c.pack, c.value) for c in claims], [("kg", 20)])

    def test_context_resolves_registered_pack_and_pack_filter(self) -> None:
        claims = scan_document(
            "docs/guide/api-reference.md",
            "`comm` registers the message kind; its eight verbs are public.\n"
            '`request(ops="verbs(pack=\\"comm\\")")` lists the eight public verbs.\n',
            PACK_COUNTS,
        )
        self.assertEqual(
            [(c.pack, c.value) for c in claims if c.kind == "pack_verbs"],
            [("comm", 8), ("comm", 8)],
        )

        claims = scan_document(
            "crates/khive-pack-comm/docs/design.md",
            "HANDLERS = COMM_HANDLERS (8 entries)\n",
            PACK_COUNTS,
        )
        self.assertEqual([(c.pack, c.value) for c in claims], [("comm", 8)])

        claims = scan_document(
            "AGENTS.md",
            "The `comm` pack contributes\neight verbs to the public surface.\n",
            PACK_COUNTS,
        )
        self.assertEqual([(c.pack, c.value) for c in claims], [("comm", 8)])

    def test_each_count_uses_nearest_pack_marker_in_same_window(self) -> None:
        claims = scan_document(
            "AGENTS.md",
            "`kg` provides twenty verbs; `comm` provides eight verbs.\n",
            PACK_COUNTS,
        )
        self.assertEqual(
            [(claim.pack, claim.value) for claim in claims],
            [("kg", 20), ("comm", 8)],
        )

        claims = scan_document(
            "README.md",
            "khive-pack-kg: graph operations (20 verbs)\n"
            "khive-pack-comm: messaging (8 verbs)\n",
            PACK_COUNTS,
        )
        self.assertEqual(
            [(claim.pack, claim.value) for claim in claims],
            [("kg", 20), ("comm", 8)],
        )

    def test_stale_second_pack_count_in_same_window_is_attributed_correctly(self) -> None:
        published = """28 verbs across 3 built-in packs.
| Pack | Verbs |
| --- | --- |
| kg | 20 |
| comm | 8 |
| workspace | 0 |
"""
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "README.md").write_text(published, encoding="utf-8")
            guide = root / "docs/guide/api.md"
            guide.parent.mkdir(parents=True)
            guide.write_text(
                "`kg` provides twenty verbs; `comm` provides seven verbs.\n",
                encoding="utf-8",
            )
            errors = validate_documented_counts(root, verbs_result())

        self.assertEqual(len(errors), 1)
        self.assertIn("claims comm verbs=7, registry says 8", errors[0])

    def test_clause_pack_marker_beats_pack_path_before_and_after_count(self) -> None:
        published = """35 verbs across 5 built-in packs.
| Pack | Verbs |
| --- | --- |
| kg | 20 |
| comm | 8 |
| workspace | 0 |
| git | 4 |
| blob | 3 |
"""
        cases = (
            (
                "pre-count",
                "`git` contributes four verbs; `blob` contributes three verbs.\n",
                "`git` contributes four verbs; `blob` contributes two verbs.\n",
                [("git", 4), ("blob", 3)],
                "claims blob verbs=2, registry says 3",
            ),
            (
                "post-count",
                "Twenty public verbs ship in the kg pack; "
                "eight public verbs ship in the comm pack.\n",
                "Twenty public verbs ship in the kg pack; "
                "seven public verbs ship in the comm pack.\n",
                [("kg", 20), ("comm", 8)],
                "claims comm verbs=7, registry says 8",
            ),
        )
        pack_path = "crates/khive-pack-workspace/README.md"
        for label, correct, stale, expected, stale_error in cases:
            with self.subTest(label=label):
                claims = scan_document(pack_path, correct, EXTENDED_PACK_COUNTS)
                self.assertEqual(
                    [(claim.pack, claim.value) for claim in claims],
                    expected,
                )

                with tempfile.TemporaryDirectory() as tmp:
                    root = Path(tmp)
                    (root / "README.md").write_text(published, encoding="utf-8")
                    pack_readme = root / pack_path
                    pack_readme.parent.mkdir(parents=True)
                    pack_readme.write_text(correct, encoding="utf-8")
                    self.assertEqual(
                        validate_documented_counts(
                            root,
                            verbs_result(EXTENDED_PACK_COUNTS),
                        ),
                        [],
                    )

                    pack_readme.write_text(stale, encoding="utf-8")
                    errors = validate_documented_counts(
                        root,
                        verbs_result(EXTENDED_PACK_COUNTS),
                    )

                self.assertEqual(len(errors), 1)
                self.assertIn(stale_error, errors[0])

    def test_named_pack_windows_cover_headings_delimiters_and_bare_names(self) -> None:
        cases = (
            ("### KG pack verbs (20 — ADR-017)\n", [("kg", 20)]),
            ("kg: 20 verbs; comm — 8 verbs.\n", [("kg", 20), ("comm", 8)]),
            ("20 kg-substrate bare verbs.\n", [("kg", 20)]),
            ("The kg substrate pack owns 20 bare verb names.\n", [("kg", 20)]),
        )
        for text, expected in cases:
            with self.subTest(text=text):
                claims = scan_document("docs/guide/api.md", text, PACK_COUNTS)
                self.assertEqual(
                    [(claim.pack, claim.value) for claim in claims],
                    expected,
                )

    def test_line_wrapped_post_count_pack_marker_beats_pack_path(self) -> None:
        text = "Twenty public verbs ship in the\nkg pack.\n"
        claims = scan_document(
            "crates/khive-pack-workspace/README.md",
            text,
            PACK_COUNTS,
        )
        self.assertEqual(
            [(claim.pack, claim.value) for claim in claims],
            [("kg", 20)],
        )

        stale = text.replace("Twenty", "Nineteen")
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "README.md").write_text(
                "28 verbs across 3 packs.\n"
                "| Pack | Verbs |\n"
                "| --- | --- |\n"
                "| kg | 20 |\n"
                "| comm | 8 |\n"
                "| workspace | 0 |\n",
                encoding="utf-8",
            )
            pack_readme = root / "crates/khive-pack-workspace/README.md"
            pack_readme.parent.mkdir(parents=True)
            pack_readme.write_text(stale, encoding="utf-8")
            errors = validate_documented_counts(root, verbs_result())

        self.assertEqual(len(errors), 1)
        self.assertIn("claims kg verbs=19, registry says 20", errors[0])

    def test_shipped_cli_help_detects_built_in_pack_count_mutations(self) -> None:
        published = """28 verbs across 3 built-in packs.
| Pack | Verbs |
| --- | --- |
| kg | 20 |
| comm | 8 |
| workspace | 0 |
"""
        help_paths = ("cli/main.ts", "cli/tests/golden/help_toplevel.txt")
        for stale_path in help_paths:
            with self.subTest(stale_path=stale_path), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                (root / "README.md").write_text(published, encoding="utf-8")
                for relative in help_paths:
                    help_file = root / relative
                    help_file.parent.mkdir(parents=True, exist_ok=True)
                    help_file.write_text(
                        "All 3 built-in packs load by default.\n",
                        encoding="utf-8",
                    )
                (root / stale_path).write_text(
                    "All 2 built-in packs load by default.\n",
                    encoding="utf-8",
                )
                errors = validate_documented_counts(root, verbs_result())

            self.assertEqual(len(errors), 1)
            self.assertIn(
                f"{stale_path}:1: claims total packs=2, registry says 3",
                errors[0],
            )

    def test_scanner_ignores_subset_and_reference_numbers(self) -> None:
        text = """`propose` is the one verb that requires JSON form.
[ADR-017](docs/adr/ADR-017-pack-standard.md) defines the Pack trait.
The harness has a 7-pack `--packs` default.
Three schedule verbs remain available without `comm`.
After persisting a reminder, the other three schedule verbs do not require `comm`.
"""
        self.assertEqual(
            scan_document("marketplace/khive/skills/kg/SKILL.md", text, PACK_COUNTS),
            [],
        )

    def test_merged_adrs_are_excluded(self) -> None:
        self.assertEqual(
            scan_document("docs/adr/ADR-999-history.md", "2 verbs across 1 pack\n", PACK_COUNTS),
            [],
        )

    def test_repository_validation_reports_stale_claim(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "README.md").write_text(
                "27 verbs across 3 packs.\n"
                "| Pack | Verbs |\n"
                "| --- | --- |\n"
                "| kg | 20 |\n"
                "| comm | 8 |\n"
                "| workspace | 0 |\n",
                encoding="utf-8",
            )
            errors = validate_documented_counts(root, verbs_result())
        self.assertEqual(len(errors), 1)
        self.assertIn("claims total verbs=27, registry says 28", errors[0])

    def test_repository_scan_ignores_adr_history(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            adr = root / "docs/adr/ADR-001-history.md"
            adr.parent.mkdir(parents=True)
            adr.write_text("1 verb across 1 pack\n", encoding="utf-8")
            counts = RegistryCounts(total_verbs=28, pack_verbs=PACK_COUNTS)
            self.assertEqual(scan_repository(root, counts), [])

    def assert_predicate_matches_scan(self, root: Path, markdown: set[str]) -> None:
        scanned = {
            path.relative_to(root).as_posix()
            for path in _published_files(root, [])
            if path.suffix == ".md"
        }
        predicted = {path for path in markdown if is_published_markdown(path)}
        self.assertEqual(predicted, scanned & markdown)
        self.assertTrue(predicted, "the population must contain scanned files")
        self.assertTrue(markdown - predicted, "the population must contain unscanned files")

    def test_copied_scanner_safe_path_cannot_mistake_import_failure_for_unscanned_markdown(self) -> None:
        """A repository import failure exits 1, which CI mistakes for an unscanned Markdown path."""
        source = Path(__file__).with_name("documented_verb_counts.py")
        env = {key: value for key, value in os.environ.items() if key != "PYTHONPATH"}
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            scanner = root / "tests" / source.name
            scanner.parent.mkdir()
            shutil.copyfile(source, scanner)
            for relative, content, expected in (
                ("README.md", "28 verbs across 3 packs.\n", 0),
                ("README.md", "# README\n\nNo registry claims.\n", 0),
                ("notes/plain.md", "# Notes\n\nNo registry claims.\n", 1),
            ):
                with self.subTest(relative=relative, content=content, expected=expected):
                    markdown = root / relative
                    markdown.parent.mkdir(parents=True, exist_ok=True)
                    markdown.write_text(content, encoding="utf-8")
                    result = subprocess.run(
                        [sys.executable, "-P", str(scanner), "--published-markdown", relative],
                        cwd=root, env=env, text=True, capture_output=True, timeout=30, check=False,
                    )
                    self.assertEqual(result.returncode, expected, result.stderr)
                    self.assertEqual(result.stderr, "")
                    self.assertEqual(result.stdout, "")

    def test_published_markdown_predicate_matches_the_scanned_files(self) -> None:
        # ci-docs-only.sh asks this predicate about one changed path; the scan walks the
        # tree with the same globs. Paths the globs treat differently are all present.
        paths = {
            "README.md", "CONTRIBUTING.md", "npm/README.md", "npm/CHANGES.md",
            "docs/guide.md", "docs/guide/deep/counts.md", "docs/adr/ADR-001-x.md",
            "docs/adrx/notes.md", "marketplace/a/b/skill.md", "scripts/notes.md",
            "crates/README.md", "crates/khive-db/README.md", "crates/khive-db/docs/a/b.md",
            "crates/khive-db/notes.md", "crates/docs/top.md", "notes/guide.md", ".github/x.md",
        }
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for path in paths:
                (root / path).parent.mkdir(parents=True, exist_ok=True)
                (root / path).write_text("# x\n", encoding="utf-8")
            self.assert_predicate_matches_scan(root, paths)

    def test_published_markdown_predicate_matches_the_repository(self) -> None:
        root = Path(__file__).resolve().parent.parent
        listed = subprocess.run(
            ["git", "ls-files", "-z", "--", "*.md"], cwd=root, capture_output=True,
            text=True, timeout=30, check=True,
        ).stdout
        tracked = {path for path in listed.split("\0") if path}
        self.assertIn("docs/adr/README.md", tracked)
        self.assert_predicate_matches_scan(root, tracked)


if __name__ == "__main__":
    unittest.main()

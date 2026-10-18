#!/usr/bin/env python3

from pathlib import Path
import tempfile
import unittest

from documented_guide_tables import (
    check_directory_map,
    documented_params,
    missing_rows,
    registry_verbs,
    signature_params,
    split_row,
    validate_guide_verb_tables,
)


REPO_ROOT = Path(__file__).resolve().parent.parent

DIRECTORY_MAP = """## Directory map

| Path | Purpose |
| --- | --- |
| `crates/khive-a` | First crate |
| `crates/khive-b` | Second crate |
| `docs/adr/` | Decision records |

---
"""

KG_TABLE = """### KG pack verbs (3 — ADR-017)

Prose before the table.

| Verb | Args | What it does |
| --- | --- | --- |
| `create` | `kind=<substrate\\|granular>` + fields | Create a record |
| `get` | `id` (UUID) | Fetch a record |
| `stats` | — | Counts |
"""

GTD_TABLE = """### GTD pack verbs (1 — ADR-019)

| Verb | Args | What it does |
| --- | --- | --- |
| `gtd.complete` | `id`, `status?` (`done` or `cancelled`), `result?` | Close a task |
"""

REGISTRY = {
    "verbs": [
        {"verb": "create", "pack": "kg", "signature": "create(kind, name?, content?)"},
        {"verb": "get", "pack": "kg", "signature": "get(id, kind?)"},
        {"verb": "stats", "pack": "kg", "signature": "stats()"},
        {"verb": "gtd.complete", "pack": "gtd", "signature": "gtd.complete(id, status?, result?)"},
        {"verb": "comm.send", "pack": "comm", "signature": "comm.send(to, content)"},
        {"verb": "mount.tool", "pack": "mount", "signature": "mount.tool(...)"},
    ]
}


class GuideFixture:
    def __init__(self, text: str, crates: tuple[str, ...] = ("khive-a", "khive-b")) -> None:
        self._dir = tempfile.TemporaryDirectory()
        self.root = Path(self._dir.name)
        (self.root / "CLAUDE.md").write_text(text, encoding="utf-8")
        (self.root / "docs" / "adr").mkdir(parents=True)
        for name in crates:
            (self.root / "crates" / name).mkdir(parents=True)
            (self.root / "crates" / name / "Cargo.toml").write_text(
                f'[package]\nname = "{name}"\ndescription = "The {name} crate"\n',
                encoding="utf-8",
            )

    def __enter__(self) -> Path:
        return self.root

    def __exit__(self, *_: object) -> None:
        self._dir.cleanup()


def guide(*sections: str) -> str:
    return "# Guide\n\n" + "\n".join(sections)


class ParsingTest(unittest.TestCase):
    def test_split_row_keeps_escaped_pipe_in_its_cell(self) -> None:
        self.assertEqual(
            split_row("| `create` | `kind=<a\\|b>` + fields | Create |"),
            ("`create`", "`kind=<a|b>` + fields", "Create"),
        )

    def test_split_row_refuses_a_non_row(self) -> None:
        with self.assertRaises(ValueError):
            split_row("not a row")

    def test_documented_params_skips_parenthesized_annotations(self) -> None:
        self.assertEqual(
            documented_params("`id`, `status?` (`done` or `cancelled`), `result?`"),
            [("id", False), ("status", True), ("result", True)],
        )
        self.assertEqual(documented_params("`kind=<substrate|granular>` + fields"), [("kind", False)])
        self.assertEqual(documented_params("—"), [])

    def test_signature_params_marks_required_and_optional(self) -> None:
        self.assertEqual(signature_params("get", "get(id, kind?)"), {"id": True, "kind": False})
        self.assertEqual(signature_params("stats", "stats()"), {})

    def test_signature_params_refuses_another_verb_or_an_opaque_list(self) -> None:
        with self.assertRaises(ValueError):
            signature_params("get", "list(kind)")
        with self.assertRaises(ValueError):
            signature_params("mount.tool", "mount.tool(...)")

    def test_registry_verbs_refuses_an_entry_without_a_signature(self) -> None:
        with self.assertRaisesRegex(ValueError, "signature"):
            registry_verbs({"verbs": [{"verb": "get", "pack": "kg"}]})
        with self.assertRaisesRegex(ValueError, "'verbs' list"):
            registry_verbs({"total": 0})


class VerbTableTest(unittest.TestCase):
    def errors(self, *sections: str) -> list[str]:
        with GuideFixture(guide(*sections)) as root:
            return validate_guide_verb_tables(root, REGISTRY)

    def test_matching_tables_report_nothing(self) -> None:
        self.assertEqual(self.errors(KG_TABLE, GTD_TABLE), [])

    def test_missing_row_is_reported(self) -> None:
        table = KG_TABLE.replace("| `stats` | — | Counts |\n", "")
        self.assertEqual(
            self.errors(table),
            ["CLAUDE.md:3: the kg table has no row for stats"],
        )

    def test_row_for_a_verb_the_registry_lacks_is_reported(self) -> None:
        table = KG_TABLE + "| `retired` | — | Gone |\n"
        errors = self.errors(table)
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("retired is not a kg verb in the registry", errors[0])

    def test_unknown_parameter_is_reported(self) -> None:
        table = KG_TABLE.replace("`id` (UUID)", "`id`, `namespace?`")
        errors = self.errors(table)
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("get has no parameter 'namespace'", errors[0])

    def test_required_parameter_written_optional_is_reported(self) -> None:
        table = KG_TABLE.replace("`id` (UUID)", "`id?`")
        errors = self.errors(table)
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("get requires 'id' but the guide writes id?", errors[0])

    def test_optional_parameter_written_bare_is_accepted(self) -> None:
        table = KG_TABLE.replace("`id` (UUID)", "`id`, `kind`")
        self.assertEqual(self.errors(table), [])

    def test_duplicate_row_is_reported(self) -> None:
        table = KG_TABLE + "| `get` | `id` | Again |\n"
        errors = self.errors(table)
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("get is listed twice", errors[0])

    def test_verb_cell_that_is_not_one_code_span_is_reported(self) -> None:
        table = KG_TABLE.replace("| `stats` |", "| stats |")
        errors = self.errors(table)
        self.assertIn("verb cell is not one code span: 'stats'", "\n".join(errors))
        self.assertIn("the kg table has no row for stats", "\n".join(errors))

    def test_table_for_a_pack_with_no_verbs_is_reported(self) -> None:
        table = GTD_TABLE.replace("### GTD pack", "### Ghost pack")
        self.assertEqual(
            self.errors(table),
            ["CLAUDE.md:3: no loaded pack 'ghost' reports verbs"],
        )

    def test_heading_with_no_table_rows_is_reported(self) -> None:
        errors = self.errors("### KG pack verbs (0)\n\nNo table here.\n")
        self.assertEqual(errors, ["CLAUDE.md:3: the kg verb table has no rows"])

    def test_guide_with_no_verb_tables_is_reported(self) -> None:
        self.assertEqual(
            self.errors("## Nothing\n"),
            ["CLAUDE.md has no '### <Name> pack verbs' table"],
        )

    def test_missing_rows_prints_registry_parameters(self) -> None:
        table = KG_TABLE.replace("| `get` | `id` (UUID) | Fetch a record |\n", "")
        with GuideFixture(guide(DIRECTORY_MAP, table), crates=("khive-a", "khive-b", "khive-c")) as root:
            self.assertEqual(
                missing_rows(root, REGISTRY),
                [
                    "| `crates/khive-c` | The khive-c crate |",
                    "| `get` | `id`, `kind?` | |",
                ],
            )


class DirectoryMapTest(unittest.TestCase):
    def errors(self, text: str, crates: tuple[str, ...] = ("khive-a", "khive-b")) -> list[str]:
        with GuideFixture(text, crates) as root:
            return check_directory_map(root)

    def test_matching_map_reports_nothing(self) -> None:
        self.assertEqual(self.errors(guide(DIRECTORY_MAP)), [])

    def test_crate_without_a_row_is_reported(self) -> None:
        self.assertEqual(
            self.errors(guide(DIRECTORY_MAP), crates=("khive-a", "khive-b", "khive-c")),
            ["CLAUDE.md: the directory map has no row for crates/khive-c"],
        )

    def test_row_for_a_crate_that_is_not_in_the_tree_is_reported(self) -> None:
        errors = self.errors(guide(DIRECTORY_MAP), crates=("khive-a",))
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("crates/khive-b has no Cargo.toml", errors[0])

    def test_row_for_a_missing_path_is_reported(self) -> None:
        text = guide(DIRECTORY_MAP.replace("`docs/adr/`", "`docs/gone/`"))
        errors = self.errors(text)
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("docs/gone/ does not exist", errors[0])

    def test_duplicate_crate_row_is_reported(self) -> None:
        text = guide(DIRECTORY_MAP.replace("| `docs/adr/`", "| `crates/khive-a` | Again |\n| `docs/adr/`"))
        errors = self.errors(text)
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("crates/khive-a is listed twice", errors[0])

    def test_guide_without_a_directory_map_is_refused(self) -> None:
        with self.assertRaisesRegex(ValueError, "Directory map"):
            self.errors(guide(KG_TABLE))

    def test_empty_tree_is_reported(self) -> None:
        self.assertEqual(
            self.errors(guide(DIRECTORY_MAP), crates=()),
            ["no crates/*/Cargo.toml found under the repository root"],
        )

    def test_repository_directory_map_matches_the_tree(self) -> None:
        self.assertEqual(check_directory_map(REPO_ROOT), [])


if __name__ == "__main__":
    unittest.main()

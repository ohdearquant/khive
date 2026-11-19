#!/usr/bin/env python3
# Static synthetic-tree regressions; no SQL engine or repository scan.

import importlib.util
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock


SCRIPT = Path(__file__).resolve().parents[1] / "sql-source-census.py"
SPEC = importlib.util.spec_from_file_location("sql_source_census", SCRIPT)
CENSUS = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = CENSUS
SPEC.loader.exec_module(CENSUS)


class RecordingTree(CENSUS.DirectoryTree):
    def __init__(self, root, crates):
        super().__init__(root, crates)
        self.reads = []

    def read(self, path):
        self.reads.append(path)
        return super().read(path)


class CensusTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.write(
            "crates/demo/Cargo.toml", '[package]\nname="demo"\nversion="0.1.0"\n'
        )

    def write(self, path, text):
        target = self.root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text, encoding="utf-8")

    def scan(self, source=None, crates=None):
        if source is not None:
            self.write("crates/demo/src/lib.rs", source)
        tree = RecordingTree(self.root, crates or ["demo"])
        report = CENSUS.census(tree, "a" * 40)
        return report, tree

    def test_comments_chars_raw_strings_and_escapes_have_lexical_boundaries(self):
        source = r"""
// "SELECT * FROM line_comment"
/* outer /* "DELETE FROM nested_comment" */ end */
const CHAR: char = '"';
fn lifetime<'a>(value: &'a str) -> &'a str { value }
const ONE: &str = r##"SELECT * FROM notes WHERE body = '"#text'"##;
const TWO: &str = "UPDATE tasks SET title = \"title\" WHERE id = ?1";
const PROSE: &str = "escaped quote: \"SELECT * FROM not_a_query\"";
"""
        report, _ = self.scan(source)
        rows = report["crates"][0]["inline_candidates"]
        self.assertEqual(
            [(row["head"], row["table_tokens"]) for row in rows],
            [("SELECT", ["notes"]), ("UPDATE", ["tasks"])],
        )
        self.assertEqual([row["line"] for row in rows], [6, 7])

    def test_english_is_not_sql_and_fragments_are_not_complete_statements(self):
        for english in [
            "update",
            "update the documentation",
            "with reason",
            "with good reason",
            "SELECT a topic",
        ]:
            with self.subTest(english=english):
                shape = CENSUS.sql_shape(english)
                self.assertTrue(shape is None or shape["shape"] == "fragment")
        self.assertIsNone(CENSUS.sql_shape("update settings set expectations"))
        self.assertEqual(
            CENSUS.sql_shape("UPDATE tasks SET priority = ?1")["shape"],
            "statement_candidate",
        )
        self.assertEqual(
            CENSUS.sql_shape("SELECT * FROM notes")["table_tokens"], ["notes"]
        )
        self.assertEqual(
            CENSUS.sql_shape(
                "WITH chosen AS (SELECT id FROM notes) SELECT * FROM chosen"
            )["head"],
            "WITH",
        )
        for fragment in [
            "SELECT {columns} FROM notes",
            "FROM notes",
            "WHERE id = ?1",
            "WITH {clauses}",
        ]:
            with self.subTest(fragment=fragment):
                self.assertEqual(CENSUS.sql_shape(fragment)["shape"], "fragment")
        self.assertEqual(CENSUS.sql_shape("SELECT 1")["shape"], "statement_candidate")

    def test_cfg_test_items_are_removed_before_literal_classification(self):
        report, _ = self.scan(r"""
#[cfg(test)] const TEST_ONLY: &str = "SELECT * FROM test_const";
#[cfg(all(unix, test))] fn helper() { let s = "DELETE FROM test_fn"; }
#[test] fn ordinary_test() { let s = "SELECT * FROM test_attr"; }
#[tokio::test(flavor = "current_thread")] async fn async_test() { let s = "SELECT * FROM async_test"; }
#[cfg(test)] mod inline_tests { const S: &str = "SELECT * FROM inline_test"; }
#[cfg(not(test))] const PROD: &str = "SELECT * FROM production";
#[doc = "SELECT * FROM doc_attribute"] pub struct Documented;
""")
        rows = report["crates"][0]["inline_candidates"]
        self.assertEqual([row["table_tokens"] for row in rows], [["production"]])

    def test_test_only_external_and_unreferenced_files_are_never_read(self):
        self.write("crates/demo/src/tests.rs", "not even lexically valid Rust: /*")
        self.write(
            "crates/demo/tests/acceptance.rs",
            'const SQL: &str = "SELECT * FROM integration_only";',
        )
        self.write(
            "crates/demo/src/forgotten.rs", 'const SQL: &str = "SELECT * FROM orphan";'
        )
        self.write(
            "crates/demo/src/production.rs",
            'const SQL: &str = "SELECT * FROM production";',
        )
        report, tree = self.scan("#[cfg(test)] mod tests;\nmod production;\n")
        row = report["crates"][0]
        self.assertEqual(
            row["rust_files"],
            ["crates/demo/src/lib.rs", "crates/demo/src/production.rs"],
        )
        self.assertEqual(row["counts"]["rust_files_not_followed"], 3)
        self.assertNotIn("crates/demo/src/tests.rs", tree.reads)
        self.assertNotIn("crates/demo/tests/acceptance.rs", tree.reads)
        self.assertEqual(row["inline_candidates"][0]["table_tokens"], ["production"])

    def test_inner_cfg_test_and_unknown_cfg_are_reported_honestly(self):
        self.write(
            "crates/demo/src/test_file.rs",
            '#![cfg(test)]\nconst S: &str = "SELECT * FROM hidden";',
        )
        report, _ = self.scan(r"""
mod test_file;
mod hidden { #![cfg(test)] const S: &str = "SELECT * FROM inline_hidden"; }
#[cfg(any(test, feature = "optional"))] const MAYBE: &str = "SELECT * FROM conditional";
#[cfg_attr(feature = "optional", allow(dead_code))] const KEPT: &str = "SELECT * FROM cfg_attr";
""")
        row = report["crates"][0]
        self.assertEqual(
            [r["table_tokens"] for r in row["inline_candidates"]],
            [["conditional"], ["cfg_attr"]],
        )
        self.assertEqual(
            [d["kind"] for d in row["diagnostics"]],
            ["unresolved_cfg", "unresolved_cfg_attr"],
        )

    def test_module_path_and_inline_module_resolution(self):
        self.write(
            "crates/demo/src/container.rs",
            '#[path="other.rs"] mod renamed;\nmod nested { mod leaf; }',
        )
        self.write(
            "crates/demo/src/other.rs", 'const A: &str = "SELECT * FROM explicit_path";'
        )
        self.write(
            "crates/demo/src/container/nested/leaf.rs",
            'const B: &str = "SELECT * FROM nested_path";',
        )
        report, _ = self.scan("mod container;")
        self.assertEqual(
            report["crates"][0]["rust_files"],
            [
                "crates/demo/src/container.rs",
                "crates/demo/src/container/nested/leaf.rs",
                "crates/demo/src/lib.rs",
                "crates/demo/src/other.rs",
            ],
        )
        self.assertEqual(report["crates"][0]["diagnostics"], [])

    def test_sql_assets_and_readers_are_different_source_units(self):
        self.write("crates/demo/sql/read.sql", "SELECT * FROM notes;\n")
        self.write(
            "crates/demo/sql/unreferenced.sql",
            "-- primary asset even without a reader\nUPDATE tasks SET done = 1;\n",
        )
        self.write("crates/demo/sql/blank.sql", "-- no SQL statement\n")
        report, _ = self.scan(
            'const A: &str = sql!("read");\nconst B: &str = include_str!("../sql/read.sql");\nconst C: &str = sql!("missing");\nconst D: &str = sql!(computed);\n'
        )
        row = report["crates"][0]
        self.assertEqual(row["counts"]["sql_assets"], 3)
        self.assertEqual(row["counts"]["inline_statement_candidates"], 0)
        self.assertEqual(row["counts"]["reader_references"], 4)
        self.assertEqual(row["counts"]["unresolved_readers"], 2)
        self.assertEqual(
            [
                (r["line"], r["macro"], r["asset"], r["resolution"])
                for r in row["reader_references"]
            ],
            [
                (1, "sql!", "crates/demo/sql/read.sql", "resolved"),
                (2, "include_str!", "crates/demo/sql/read.sql", "resolved"),
                (3, "sql!", "crates/demo/sql/missing.sql", "unresolved"),
                (4, "sql!", None, "unresolved"),
            ],
        )
        self.assertIsNone(row["sql_assets"][0]["classification"])

    def test_test_reader_is_not_counted_and_sql_asset_remains_primary(self):
        self.write("crates/demo/sql/test.sql", "SELECT 1;")
        report, _ = self.scan(
            '#[cfg(test)] mod tests { const S: &str = sql!("test"); }'
        )
        counts = report["crates"][0]["counts"]
        self.assertEqual((counts["sql_assets"], counts["reader_references"]), (1, 0))

    def test_literal_include_is_followed_and_dynamic_include_is_diagnosed(self):
        self.write(
            "crates/demo/src/generated.rs", 'const S: &str = "SELECT * FROM included";'
        )
        report, _ = self.scan(
            'include!("generated.rs");\ninclude!(concat!(env!("OUT_DIR"), "/generated.rs"));'
        )
        row = report["crates"][0]
        self.assertEqual(row["inline_candidates"][0]["table_tokens"], ["included"])
        self.assertEqual(
            [d["kind"] for d in row["diagnostics"]], ["unresolved_include"]
        )

    def test_explicit_crate_filter_never_reads_another_crate(self):
        self.write("crates/forbidden/Cargo.toml", "not a valid manifest")
        self.write("crates/forbidden/src/lib.rs", "/* unterminated")
        self.write("crates/forbidden/sql/secret.sql", "DELETE FROM forbidden;")
        report, tree = self.scan('const S: &str = "SELECT * FROM allowed";')
        self.assertEqual([c["crate"] for c in report["crates"]], ["demo"])
        self.assertTrue(all(path.startswith("crates/demo/") for path in tree.reads))
        self.assertEqual(
            report["crates"][0]["inline_candidates"][0]["table_tokens"], ["allowed"]
        )

    def test_cargo_declared_roots_exclude_tests_examples_and_benches(self):
        self.write(
            "crates/demo/Cargo.toml",
            '[package]\nname="demo"\nversion="0.1.0"\nautobins=false\nbuild=false\n[lib]\npath="core/entry.rs"\n[[bin]]\nname="admin"\npath="bin/admin.rs"\n[[test]]\nname="fixture"\npath="fixtures/custom.rs"\n',
        )
        self.write("crates/demo/core/entry.rs", 'const S: &str = "SELECT 1";')
        self.write("crates/demo/bin/admin.rs", 'const S: &str = "SELECT 2";')
        self.write("crates/demo/src/main.rs", "/* not a root */")
        self.write("crates/demo/fixtures/custom.rs", "/*")
        self.write("crates/demo/examples/demo.rs", "/*")
        self.write("crates/demo/benches/demo.rs", "/*")
        report, _ = self.scan()
        self.assertEqual(
            report["crates"][0]["rust_files"],
            ["crates/demo/bin/admin.rs", "crates/demo/core/entry.rs"],
        )

    def test_missing_ambiguous_and_outside_modules_are_explicit(self):
        self.write("crates/demo/src/ambiguous.rs", "")
        self.write("crates/demo/src/ambiguous/mod.rs", "")
        report, _ = self.scan(
            'mod missing; mod ambiguous; #[path="../../other/hidden.rs"] mod outside;'
        )
        notices = report["crates"][0]["diagnostics"]
        self.assertEqual([d["kind"] for d in notices], ["unresolved_module"] * 3)
        self.assertEqual(report["crates"][0]["counts"]["rust_files_read"], 1)

    def test_reports_are_deterministic_and_label_static_limits(self):
        first, _ = self.scan('const S: &str = "SELECT 1";')
        second, _ = self.scan()
        self.assertEqual(
            json.dumps(first, sort_keys=True), json.dumps(second, sort_keys=True)
        )
        text = CENSUS.text_report(first)
        self.assertIn("not executed SQL", text)
        self.assertIn("assets=0 inline=1 fragments=0 readers=0", text)
        self.assertIn("macros not expanded", text)

    def test_revision_and_crate_filters_reject_unpinned_or_escaping_inputs(self):
        for revision in ["HEAD", "abc123", "a" * 39, "g" * 40, "a" * 40 + ":file"]:
            with self.subTest(revision=revision), self.assertRaises(CENSUS.CensusError):
                CENSUS.full_revision(revision)
        self.assertEqual(CENSUS.full_revision("A" * 40), "a" * 40)
        for name in ["../outside", "a/b", ".", "-x/../../bad"]:
            with self.subTest(name=name), self.assertRaises(CENSUS.CensusError):
                CENSUS.crate_name(name)

    def test_git_adapter_uses_only_read_only_argv_and_selected_paths(self):
        responses = [
            b"a" * 40 + b"\n",
            b"100644 blob " + b"b" * 40 + b"\tcrates/demo/Cargo.toml\0",
            b'[package]\nname="demo"\nversion="0.1.0"\n',
        ]

        def result(*args, **kwargs):
            return mock.Mock(returncode=0, stdout=responses.pop(0), stderr=b"")

        with mock.patch.object(CENSUS.subprocess, "run", side_effect=result) as run:
            tree = CENSUS.GitTree(Path("/fixture"), "a" * 40, ["demo"])
            self.assertIn("name", tree.read("crates/demo/Cargo.toml"))
        commands = [call.args[0] for call in run.call_args_list]
        self.assertEqual(
            [command[3] for command in commands], ["rev-parse", "ls-tree", "show"]
        )
        self.assertEqual(commands[1][-1], "crates/demo/")
        self.assertTrue(all("shell" not in call.kwargs for call in run.call_args_list))
        with self.assertRaises(CENSUS.CensusError):
            tree.read("crates/forbidden/src/lib.rs")

    def test_bare_transaction_vocabulary_is_not_inline_sql(self):
        report, _ = self.scan("""
const LABELS: [&str; 6] = ["commit", "begin", "rollback", "release", "savepoint", "vacuum"];
const SQL: [&str; 3] = ["COMMIT;", "COMMIT TRANSACTION", "BEGIN IMMEDIATE"];
""")
        rows = report["crates"][0]["inline_candidates"]
        self.assertEqual([row["head"] for row in rows], ["COMMIT", "COMMIT", "BEGIN"])
        self.assertEqual(
            report["crates"][0]["counts"]["inline_statement_candidates"], 3
        )
        for word in ["commit", "begin", "rollback", "release", "savepoint", "vacuum"]:
            with self.subTest(word=word):
                self.assertIsNone(CENSUS.sql_shape(word))

    def test_cfg_test_initializers_are_removed_through_top_level_semicolon(self):
        report, _ = self.scan("""
#[cfg(test)]
const CONDITIONAL: &str = if true { "SELECT * FROM first" } else { "SELECT * FROM second" };
#[cfg(test)]
pub(crate) static MATCHED: &str = match 1 { 1 => { "SELECT * FROM match_arm" }, _ => "SELECT * FROM fallback" };
#[cfg(test)]
pub const unsafe fn helper() -> &'static str { "SELECT * FROM const_fn" }
const PRODUCTION: &str = "SELECT * FROM production";
""")
        self.assertEqual(
            [r["table_tokens"] for r in report["crates"][0]["inline_candidates"]],
            [["production"]],
        )

    def test_explicit_cargo_roots_must_exist_inside_selected_crate(self):
        cases = [
            ('[lib]\npath="missing.rs"\n', "library", "missing"),
            ('[lib]\npath="../other/lib.rs"\n', "library", "outside"),
            ("[lib]\n", "library", "missing"),
            ('[[bin]]\nname="admin"\npath="missing.rs"\n', "binary", "missing"),
            ('[[bin]]\nname="admin"\npath="../other/main.rs"\n', "binary", "outside"),
        ]
        header = '[package]\nname="demo"\nversion="0.1.0"\n'
        for section, label, problem in cases:
            with self.subTest(section=section):
                self.write("crates/demo/Cargo.toml", header + section)
                with self.assertRaisesRegex(
                    CENSUS.CensusError, f"explicit {label} path is {problem}"
                ):
                    self.scan()
        for build, problem in [
            ("missing.rs", "missing"),
            ("../other/build.rs", "outside"),
        ]:
            with self.subTest(build=build):
                self.write("crates/demo/Cargo.toml", header + f'build="{build}"\n')
                with self.assertRaisesRegex(
                    CENSUS.CensusError, f"explicit build script path is {problem}"
                ):
                    self.scan()

    def test_text_report_escapes_controls_in_every_printed_string_position(self):
        plain = {"revision": r"café 医学 \n", "crates": [], "limits": []}
        self.assertEqual(
            CENSUS.text_report(plain),
            r"Static SQL source census at café 医学 \n (not executed SQL)" + "\n",
        )
        fields = (
            "revision", "crate", "asset", "candidate", "shape", "head",
            "reader", "macro", "resolution", "reader_asset", "notice",
            "kind", "detail", "limit",
        )
        cases = (
            ("Cc LF", "\n", r"\u000a"),
            ("Cc TAB", "\t", r"\u0009"),
            ("Cc ESC", "\x1b", r"\u001b"),
            ("Cc DEL", "\x7f", r"\u007f"),
            ("Cf BMP", "\u202e", r"\u202e"),
            ("Cf astral", "\U000e0001", r"\U000e0001"),
            ("Zl", "\u2028", r"\u2028"),
            ("Zp", "\u2029", r"\u2029"),
        )
        for category, unsafe, escaped in cases:
            with self.subTest(category=category):
                def value(field):
                    return f"{field}{unsafe}:café 医学" + r"\n"

                report = {
                    "revision": value("revision"),
                    "limits": [value("limit")],
                    "crates": [{
                        "crate": value("crate"),
                        "counts": {
                            "sql_assets": 1, "inline_statement_candidates": 1,
                            "inline_fragments": 0, "reader_references": 1,
                            "unresolved_readers": 0, "rust_files_read": 1,
                        },
                        "sql_assets": [{"path": value("asset")}],
                        "inline_candidates": [{
                            "path": value("candidate"), "line": 2, "column": 3,
                            "shape": value("shape"), "head": value("head"),
                        }],
                        "reader_references": [{
                            "path": value("reader"), "line": 4,
                            "macro": value("macro"),
                            "resolution": value("resolution"),
                            "asset": value("reader_asset"),
                        }],
                        "diagnostics": [{
                            "path": value("notice"), "line": 5,
                            "kind": value("kind"), "detail": value("detail"),
                        }],
                    }],
                }
                original = json.dumps(report, sort_keys=True)
                text = CENSUS.text_report(report)
                for field in fields:
                    self.assertIn(f"{field}{escaped}:café 医学" + r"\n", text)
                self.assertEqual(text.count("\n"), 7)
                self.assertEqual(len(text.splitlines()), 7)
                self.assertTrue(text.endswith("\n"))
                if unsafe != "\n":
                    self.assertNotIn(unsafe, text)
                self.assertEqual(json.dumps(report, sort_keys=True), original)

    def test_main_json_output_and_report_object_are_unchanged(self):
        report = {
            "revision": "a" * 40,
            "crates": [],
            "limits": ["café 医学\n\x1b\u202e\U000e0001\u2028\u2029"],
        }
        expected = json.dumps(report, indent=2, sort_keys=True) + "\n"
        original = json.dumps(report, sort_keys=True)
        with (
            mock.patch.object(CENSUS, "GitTree") as make_tree,
            mock.patch.object(CENSUS, "census", return_value=report) as scan,
            mock.patch.object(CENSUS, "text_report", side_effect=AssertionError("JSON used text rendering")),
            mock.patch.object(CENSUS.sys, "stdout", new_callable=io.StringIO) as stdout,
        ):
            make_tree.return_value.revision = "a" * 40
            result = CENSUS.main([
                "--repo", "/fixture", "--revision", "a" * 40,
                "--crate", "demo", "--format", "json",
            ])
            self.assertEqual(result, 0)
            self.assertEqual(stdout.getvalue(), expected)
            scan.assert_called_once_with(make_tree.return_value, "a" * 40)
        self.assertEqual(json.dumps(report, sort_keys=True), original)

    def test_main_caught_errors_escape_terminal_controls_on_stderr(self):
        diagnostic = "bad\n\t\x1b\u202e\U000e0001\u2028\u2029:café 医学"
        expected = (
            r"sql-source-census: bad\u000a\u0009\u001b\u202e\U000e0001\u2028\u2029:café 医学"
            + "\n"
        )
        for output_format in ("text", "json"):
            with self.subTest(output_format=output_format):
                with (
                    mock.patch.object(CENSUS, "GitTree", side_effect=CENSUS.CensusError(diagnostic)),
                    mock.patch.object(CENSUS.sys, "stdout", new_callable=io.StringIO) as stdout,
                    mock.patch.object(CENSUS.sys, "stderr", new_callable=io.StringIO) as stderr,
                ):
                    result = CENSUS.main([
                        "--repo", "/fixture", "--revision", "a" * 40,
                        "--crate", "demo", "--format", output_format,
                    ])
                    self.assertEqual(result, 2)
                    self.assertEqual(stdout.getvalue(), "")
                    self.assertEqual(stderr.getvalue(), expected)



if __name__ == "__main__":
    unittest.main()

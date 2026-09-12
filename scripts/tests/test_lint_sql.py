"""SQL fragment dependencies and isolation in the repository lint."""
import pathlib
import re
import shutil
import subprocess
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]

# lint-sql.sh classifies a file by its first keyword, never by its name or its
# directory. The rule is mirrored here because this fixture has to select the same
# population the fragment groups do.
DDL = re.compile(r"\s*(CREATE|ALTER|DROP|PRAGMA|BEGIN|COMMIT)\b", re.IGNORECASE)


def declares_schema(path):
    for line in path.read_text().splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("--"):
            continue
        return bool(DDL.match(line))
    return True  # an empty file declares nothing and prepares nothing


class SqlLintTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="lint-sql-")
        self.addCleanup(self.temporary.cleanup)
        self.root = pathlib.Path(self.temporary.name)
        (self.root / "scripts").mkdir()
        (self.root / "crates").mkdir()
        shutil.copy2(ROOT / "scripts/lint-sql.sh", self.root / "scripts/lint-sql.sh")

    def run_lint(self):
        return subprocess.run(
            ["sh", str(self.root / "scripts/lint-sql.sh")],
            text=True, capture_output=True, timeout=20, check=False,
        )

    def copy_git_fragments(self):
        """Copy khive-pack-git's schema fragments, and only those.

        This copied the whole `sql/` directory until that directory also held
        query statements extracted out of Rust. A query is prepared against the
        migration chain, which an isolated tree does not carry, so copying
        everything reddened the fixture over eleven tables the chain declares —
        a true statement about the fixture and nothing at all about the question
        these tests ask. The population here is the fragments: select them by the
        linter's own rule, and count what was copied instead of a literal that
        goes stale the next time a statement lands in that directory.
        """
        source = ROOT / "crates/khive-pack-git/sql"
        destination = self.root / "crates/khive-pack-git/sql"
        destination.mkdir(parents=True)
        names = []
        for path in sorted(source.glob("*.sql")):
            if declares_schema(path):
                shutil.copy2(path, destination / path.name)
                names.append(path.name)
        # The subject is an index resolving a table declared in a different file,
        # so a pass means nothing unless both are in the fixture.
        self.assertIn("git_receipts.sql", names)
        self.assertTrue(
            any(name.endswith("_index.sql") or "_index_" in name for name in names),
            f"no index fragment among {names}, so this fixture cannot show "
            "one file resolving a table another file declares",
        )
        return len(names)

    def test_real_git_indexes_see_their_table(self):
        copied = self.copy_git_fragments()
        result = self.run_lint()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        # The whole line, so a count is never a substring of a larger one, and the
        # prepared column asserts the fixture holds no query files.
        self.assertIn(
            f"SQL lint: {copied} file(s) OK (0 prepared, {copied} executed)",
            result.stdout,
        )

    def test_broken_fragment_is_still_refused(self):
        self.copy_git_fragments()
        broken = self.root / "crates/khive-pack-git/sql/zz-broken.sql"
        broken.write_text("CREATE INDEX broken ON absent_table(missing);\n")
        result = self.run_lint()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("zz-broken.sql: FAILED", result.stdout)
        self.assertIn("no such table", result.stdout)

    def test_a_query_file_is_prepared_against_the_schema_it_reads(self):
        """A statement file resolves names the DDL beside it declares, and only those.

        The extraction program's whole premise is that a moved statement is
        checked rather than merely stored, so both arms belong here: one query
        over a table the fixture declares passes, and one over a table it does
        not is refused by name.
        """
        directory = self.root / "crates/reader/sql"
        directory.mkdir(parents=True)
        (directory / "00-table.sql").write_text(
            "CREATE TABLE rows_seen (\n    id INTEGER,\n    seen_at TEXT\n);\n"
        )
        (directory / "by_id_select.sql").write_text(
            "SELECT seen_at FROM rows_seen WHERE id = ?1;\n"
        )
        result = self.run_lint()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("SQL lint: 2 file(s) OK (1 prepared, 1 executed)", result.stdout)

        (directory / "absent_select.sql").write_text(
            "SELECT seen_at FROM rows_never_seen WHERE id = ?1;\n"
        )
        result = self.run_lint()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("absent_select.sql: FAILED to prepare", result.stdout)
        self.assertIn("no such table: rows_never_seen", result.stdout)

    def test_each_directory_has_an_independent_database(self):
        for name in ["first", "second"]:
            directory = self.root / "crates" / name / "sql"
            directory.mkdir(parents=True)
            (directory / "00-table.sql").write_text("CREATE TABLE shared (id INTEGER);\n")
            (directory / "01-index.sql").write_text("CREATE INDEX by_id ON shared(id);\n")
        # A one-file directory still loads by itself.
        directory = self.root / "crates/single/sql"
        directory.mkdir(parents=True)
        (directory / "table.sql").write_text("CREATE TABLE shared (id INTEGER);\n")
        result = self.run_lint()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("5 file(s) OK", result.stdout)


if __name__ == "__main__":
    unittest.main()

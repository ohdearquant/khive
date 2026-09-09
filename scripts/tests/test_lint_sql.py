"""SQL fragment dependencies and isolation in the repository lint."""
import pathlib
import shutil
import subprocess
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[2]


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
        shutil.copytree(ROOT / "crates/khive-pack-git/sql", self.root / "crates/khive-pack-git/sql")

    def test_real_git_indexes_see_their_table(self):
        self.copy_git_fragments()
        result = self.run_lint()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("3 file(s) OK", result.stdout)

    def test_broken_fragment_is_still_refused(self):
        self.copy_git_fragments()
        broken = self.root / "crates/khive-pack-git/sql/zz-broken.sql"
        broken.write_text("CREATE INDEX broken ON absent_table(missing);\n")
        result = self.run_lint()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("zz-broken.sql: FAILED", result.stdout)
        self.assertIn("no such table", result.stdout)

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

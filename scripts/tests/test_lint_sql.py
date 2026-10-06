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
        if ((self.root / "crates/khive-db/sql").exists()
                and not (self.root / "crates/khive-db/src/migrations.rs").exists()):
            self.write_registry(["schema.sql"])
        return subprocess.run(
            ["sh", str(self.root / "scripts/lint-sql.sh")],
            text=True, capture_output=True, timeout=20, check=False,
        )

    def write_registry(self, names):
        source = self.root / "crates/khive-db/src/migrations.rs"
        source.parent.mkdir(parents=True, exist_ok=True)
        constants = [
            f'const V{index}_UP: &str = include_str!("../sql/{name}");'
            for index, name in enumerate(names, 1)
        ]
        entries = [
            f'VersionedMigration {{ version: {index}, name: "fixture", up: V{index}_UP }},'
            for index in range(1, len(names) + 1)
        ]
        source.write_text("\n".join(constants)
                          + "\npub const MIGRATIONS: &[VersionedMigration] = &[\n"
                          + "\n".join(entries) + "\n];\n")

    def core_fixture(self, schema):
        core = self.root / "crates/khive-db/sql"
        core.mkdir(parents=True)
        (core / "schema.sql").write_text(schema)
        self.write_registry(["schema.sql"])
        return core

    def test_core_parameterized_insert_is_prepared_without_null_execution(self):
        core = self.core_fixture("CREATE TABLE deliveries (message TEXT NOT NULL);\n")
        # A numbered name is still a query when it is not registered as a migration.
        (core / "006-message-insert.sql").write_text(
            "INSERT INTO deliveries (message) VALUES (?1);\n"
        )
        result = self.run_lint()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("2 file(s) OK (1 prepared, 1 executed)", result.stdout)

    def test_core_missing_column_is_refused_during_preparation(self):
        core = self.core_fixture("CREATE TABLE deliveries (message TEXT);\n")
        (core / "message-select.sql").write_text("SELECT missing FROM deliveries;\n")
        result = self.run_lint()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("message-select.sql: FAILED to prepare", result.stdout)
        self.assertIn("no such column: missing", result.stdout)

    def test_registered_data_reconciliation_runs_before_its_unique_index(self):
        core = self.core_fixture(
            "CREATE TABLE deliveries (message TEXT NOT NULL);\n"
            "INSERT INTO deliveries VALUES ('same'), ('same');\n"
        )
        (core / "002-delivery-status.sql").write_text(
            "ALTER TABLE deliveries ADD COLUMN ready INTEGER DEFAULT 1;\n"
        )
        # Registration, not the filename or first keyword, makes this a migration.
        (core / "reconcile-deliveries.sql").write_text(
            "DELETE FROM deliveries WHERE rowid NOT IN "
            "(SELECT MIN(rowid) FROM deliveries GROUP BY message);\n"
            "CREATE UNIQUE INDEX one_message ON deliveries(message);\n"
        )
        (core / "delivery-select.sql").write_text(
            "SELECT message FROM deliveries WHERE ready = ?1;\n"
        )
        self.write_registry(["schema.sql", "reconcile-deliveries.sql"])
        # Version constants and comments must not hide an actual up target.
        source = self.root / "crates/khive-db/src/migrations.rs"
        source.write_text("const RECONCILE_VERSION: u32 = 2;\n" + source.read_text().replace(
            "version: 2", "version: RECONCILE_VERSION /* up: NOT_A_MIGRATION */"
        ))
        result = self.run_lint()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("4 file(s) OK (1 prepared, 3 executed)", result.stdout)

    def test_core_queries_see_other_ddl_and_never_fire_write_triggers(self):
        core = self.core_fixture(
            "CREATE TABLE deliveries (message TEXT NOT NULL);\n"
            "INSERT INTO deliveries VALUES ('held');\n"
            "CREATE TRIGGER no_insert BEFORE INSERT ON deliveries "
            "BEGIN SELECT RAISE(ABORT, 'query executed INSERT'); END;\n"
            "CREATE TRIGGER no_update BEFORE UPDATE ON deliveries "
            "BEGIN SELECT RAISE(ABORT, 'query executed UPDATE'); END;\n"
            "CREATE TRIGGER no_delete BEFORE DELETE ON deliveries "
            "BEGIN SELECT RAISE(ABORT, 'query executed DELETE'); END;\n"
        )
        for name, sql in [
            ("insert", "INSERT INTO deliveries VALUES ('new');"),
            ("update", "UPDATE deliveries SET message = 'new';"),
            ("delete", "DELETE FROM deliveries;"),
        ]:
            (core / f"message-{name}.sql").write_text(sql + "\n")
        other = self.root / "crates/reader/sql"
        other.mkdir(parents=True)
        (other / "statuses.sql").write_text("CREATE TABLE statuses (ready INTEGER);\n")
        (core / "message-select.sql").write_text(
            "SELECT message FROM deliveries CROSS JOIN statuses WHERE ready = ?1;\n"
        )
        result = self.run_lint()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("6 file(s) OK (4 prepared, 2 executed)", result.stdout)

    def test_unresolved_or_missing_registered_migration_fails_closed(self):
        self.core_fixture("CREATE TABLE deliveries (message TEXT);\n")
        source = self.root / "crates/khive-db/src/migrations.rs"
        original = source.read_text()
        source.write_text(original.replace("up: V1_UP", "up: UNKNOWN_UP"))
        result = self.run_lint()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("cannot resolve registered migrations", result.stdout)
        self.assertIn("UNKNOWN_UP", result.stdout)
        source.write_text(original.replace("../sql/schema.sql", "../sql/missing.sql"))
        result = self.run_lint()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("registered migration is missing", result.stdout)

    def copy_git_fragments(self):
        """Git auxiliary DDL and live-note indexes depend on the core notes table."""
        core = self.root / "crates/khive-db/sql"
        core.mkdir(parents=True)
        (core / "schema.sql").write_text(
            "CREATE TABLE notes (\n"
            "    id TEXT PRIMARY KEY,\n"
            "    namespace TEXT,\n"
            "    kind TEXT,\n"
            "    properties TEXT,\n"
            "    deleted_at INTEGER\n"
            ");\n"
        )
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
        return len(names) + 1

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

    def test_git_core_fixture_is_scoped_and_queries_remain_prepared(self):
        copied = self.copy_git_fragments()
        git = self.root / "crates/khive-pack-git/sql"
        shutil.copy2(
            ROOT / "crates/khive-pack-git/sql/commits_by_sha_select.sql",
            git / "commits_by_sha_select.sql",
        )
        shutil.copy2(
            ROOT / "crates/khive-pack-git/sql/notes_by_number_select.sql",
            git / "notes_by_number_select.sql",
        )
        result = self.run_lint()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(
            f"SQL lint: {copied + 2} file(s) OK (2 prepared, {copied} executed)",
            result.stdout,
        )
        unrelated = self.root / "crates/unrelated/sql"
        unrelated.mkdir(parents=True)
        (unrelated / "unexpected_core_visibility.sql").write_text(
            "CREATE INDEX unexpected_core_visibility ON notes(id);\n"
        )
        result = self.run_lint()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unexpected_core_visibility.sql: FAILED to load", result.stdout)
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

    def test_real_tool_schema_uses_core_fixture_and_preserves_query_preparation(self):
        core = self.root / "crates/khive-db/sql"
        core.mkdir(parents=True)
        (core / "schema.sql").write_text(
            "CREATE TABLE entities (\n"
            "    id TEXT PRIMARY KEY,\n"
            "    namespace TEXT,\n"
            "    kind TEXT,\n"
            "    tags TEXT,\n"
            "    name TEXT,\n"
            "    created_at INTEGER,\n"
            "    deleted_at INTEGER\n"
            ");\n"
        )
        source = ROOT / "crates/khive-pack-tool/sql"
        destination = self.root / "crates/khive-pack-tool/sql"
        destination.mkdir(parents=True)
        names = []
        for path in sorted(source.glob("*.sql")):
            if declares_schema(path):
                shutil.copy2(path, destination / path.name)
                names.append(path.name)
        self.assertIn("002-grants.sql", names)
        self.assertIn("grant-invalidation.sql", names)
        (destination / "grant_marker_select.sql").write_text(
            "SELECT invalidated_at FROM tool_grants WHERE id = ?1;\n"
        )
        result = self.run_lint()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(
            f"SQL lint: {len(names) + 2} file(s) OK "
            f"(1 prepared, {len(names) + 1} executed)",
            result.stdout,
        )

        # The core fixture belongs only to the tool directory, not every pack.
        unrelated = self.root / "crates/unrelated/sql"
        unrelated.mkdir(parents=True)
        (unrelated / "unexpected_core_visibility.sql").write_text(
            "CREATE TRIGGER unexpected_core_visibility AFTER INSERT ON entities "
            "BEGIN SELECT 1; END;\n"
        )
        result = self.run_lint()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unexpected_core_visibility.sql: FAILED to load", result.stdout)
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

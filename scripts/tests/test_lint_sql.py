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

    def gtd_fixture(self):
        directory = self.root / "crates/khive-pack-gtd/sql"
        directory.mkdir(parents=True, exist_ok=True)
        statements = {
            "ddl": "CREATE TABLE IF NOT EXISTS gtd_lifecycle_audit (\n"
                   "    note_id TEXT NOT NULL,\n    from_state TEXT NOT NULL,\n"
                   "    to_state TEXT NOT NULL,\n    note TEXT,\n"
                   "    at INTEGER NOT NULL,\n    namespace TEXT\n)\n",
            "note-index": "CREATE INDEX IF NOT EXISTS idx_gtd_audit_note "
                          "ON gtd_lifecycle_audit(note_id, at DESC)\n",
            "table-info": "PRAGMA table_info(gtd_lifecycle_audit)\n",
            "add-namespace": "ALTER TABLE gtd_lifecycle_audit ADD COLUMN namespace TEXT\n",
        }
        for name, sql in statements.items():
            (directory / f"task-lifecycle-audit-{name}.sql").write_text(sql)
        (directory / "audit-insert.sql").write_text(
            "INSERT INTO gtd_lifecycle_audit "
            "(note_id, from_state, to_state, note, at, namespace) "
            "VALUES (?1, ?2, ?3, ?4, ?5, ?6)\n"
        )
        return directory

    def test_gtd_legacy_upgrade_and_canonical_query_schema_both_validate(self):
        self.gtd_fixture()
        result = self.run_lint()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("5 file(s) OK (1 prepared, 4 executed)", result.stdout)

    def test_gtd_upgrade_errors_and_wrong_layouts_are_never_suppressed(self):
        cases = [
            ("add-namespace", "ALTER TABLE absent ADD COLUMN namespace TEXT\n", "no such table"),
            ("add-namespace", "ALTER TABLE gtd_lifecycle_audit ADD COLUMN namespase TEXT\n", "after upgrade"),
            ("add-namespace", "ALTER TABLE gtd_lifecycle_audit ADD COLUMN namespace BLOB\n", "after upgrade"),
            ("add-namespace", "ALTER TABLE gtd_lifecycle_audit ADD COLUMN namespace TEXT DEFAULT 'x'\n", "after upgrade"),
            ("add-namespace", "ALTER TABLE gtd_lifecycle_audit ADD COLUMN namespace TEXT NOT NULL DEFAULT 'x'\n", "after upgrade"),
            ("add-namespace", "ALTER TABLE gtd_lifecycle_audit ADD COLMN namespace TEXT\n", "after upgrade"),
            ("add-namespace", "ALTER TABLE gtd_lifecycle_audit ADD COLUMN namespace TEXT; SELECT 1;\n", "one statement"),
            ("add-namespace", "SELECT 1\n", "must contain pack DDL"),
            ("table-info", "PRAGMA table_info(absent)\n", "before upgrade"),
        ]
        for name, sql, diagnostic in cases:
            with self.subTest(name=name, sql=sql):
                directory = self.gtd_fixture()
                (directory / f"task-lifecycle-audit-{name}.sql").write_text(sql)
                result = self.run_lint()
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn(diagnostic, result.stdout)

    def test_gtd_syntax_error_and_missing_bundle_file_fail(self):
        directory = self.gtd_fixture()
        path = directory / "task-lifecycle-audit-add-namespace.sql"
        path.write_text("ALTER TABLE gtd_lifecycle_audit ADD COLUMN (\n")
        result = self.run_lint()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("FAILED legacy fixture", result.stdout)
        self.assertIn("syntax error", result.stdout)
        path.unlink()
        result = self.run_lint()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("incomplete GTD upgrade bundle", result.stdout)

    def test_gtd_legacy_fixture_requires_the_exact_old_layout(self):
        self.gtd_fixture()
        script = self.root / "scripts/lint-sql.sh"
        original = script.read_text()
        for before, after in [
            ("CREATE TABLE gtd_lifecycle_audit (", "CREATE TABLE wrong_fixture ("),
            ("    at INTEGER NOT NULL\n)\n", "    at INTEGER NOT NULL,\n    namespace TEXT\n)\n"),
        ]:
            with self.subTest(after=after):
                self.assertEqual(original.count(before), 1)
                script.write_text(original.replace(before, after))
                result = self.run_lint()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("legacy fixture: unexpected table layout", result.stdout)
        script.write_text(original)

    def test_gtd_canonical_layout_must_match_the_upgraded_layout(self):
        directory = self.gtd_fixture()
        path = directory / "task-lifecycle-audit-ddl.sql"
        source = path.read_text()
        for broken in [source.replace(",\n    namespace TEXT", ""),
                       source.replace("namespace TEXT", "namespace BLOB")]:
            with self.subTest(broken=broken):
                path.write_text(broken)
                result = self.run_lint()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("canonical schema: unexpected table layout", result.stdout)

    def test_gtd_upgrade_registration_does_not_exempt_neighboring_ddl(self):
        directory = self.gtd_fixture()
        (directory / "zz-unregistered.sql").write_text(
            "ALTER TABLE absent ADD COLUMN missing TEXT\n"
        )
        result = self.run_lint()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("zz-unregistered.sql: FAILED to load", result.stdout)
        self.assertIn("no such table", result.stdout)

    def test_gtd_queries_keep_name_and_bind_checks_without_executing(self):
        directory = self.gtd_fixture()
        query = directory / "audit-insert.sql"
        original = query.read_text()
        for broken, diagnostic in [
            (original.replace("gtd_lifecycle_audit", "absent"), "no such table"),
            (original.replace("namespace)", "missing)"), "no column named missing"),
            (original.replace("?6", "?8"), "positional binds must run 1..N"),
        ]:
            with self.subTest(broken=broken):
                query.write_text(broken)
                result = self.run_lint()
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(diagnostic, result.stdout)
        query.write_text(original)
        (directory / "zz-no-insert.sql").write_text(
            "CREATE TRIGGER no_insert BEFORE INSERT ON gtd_lifecycle_audit "
            "BEGIN SELECT RAISE(ABORT, 'query executed INSERT'); END;\n"
        )
        result = self.run_lint()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("6 file(s) OK (1 prepared, 5 executed)", result.stdout)

    def test_gtd_fixture_keeps_ddl_local_and_current_query_schema_shared(self):
        self.gtd_fixture()
        unrelated = self.root / "crates/unrelated/sql"
        unrelated.mkdir(parents=True)
        index = unrelated / "unexpected_index.sql"
        index.write_text("CREATE INDEX unexpected ON gtd_lifecycle_audit(namespace)\n")
        result = self.run_lint()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unexpected_index.sql: FAILED to load", result.stdout)
        self.assertIn("no such table", result.stdout)
        index.unlink()
        (unrelated / "allowed_query.sql").write_text(
            "SELECT namespace FROM gtd_lifecycle_audit WHERE note_id = ?1\n"
        )
        result = self.run_lint()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("6 file(s) OK (2 prepared, 4 executed)", result.stdout)


if __name__ == "__main__":
    unittest.main()

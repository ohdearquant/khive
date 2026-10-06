#!/bin/sh
# Lint SQL DDL files: execution validation + hygiene + format.
#
# sqlfluff cannot parse the fts5/vec0 virtual-table extension syntax these files
# use (and has no working auto-formatter for it), so the checks are:
#   1. execution  — DDL files must load cleanly into in-memory SQLite (fts5 is
#                   built into the stdlib sqlite3 module). Migration files under
#                   crates/khive-db/sql/ form a forward chain: schema.sql is the
#                   V1 baseline and later NNN-<name>.sql migrations ALTER/extend
#                   it, so they are replayed cumulatively in version order on one
#                   database. Other directories each load their fragments in sorted
#                   filename order into one fresh database, so indexes see tables
#                   declared by earlier fragments. One-file directories stand alone.
#                   The tool pack's registry trigger and Git pack's live-note
#                   indexes additionally use the core migration chain as their
#                   schema fixture.
#   1b. preparation — a QUERY file (one that does not start with DDL) is prepared,
#                   not executed, against a database carrying the migration chain
#                   plus every DDL fragment in the tree. Preparation is what
#                   resolves table and column names, so a typo in either fails
#                   here rather than in a test run. Positional binds are filled
#                   with NULL and the statement runs under EXPLAIN, so nothing is
#                   evaluated and no row is touched. A file is classified by its
#                   first keyword, except for actual registered core migrations,
#                   which may begin with data reconciliation before their DDL.
#   2. hygiene    — no trailing whitespace, no tabs.
#   3. format     — multi-column CREATE TABLE must be one column per line
#                   (catches comma-jammed single-line tables).
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$SCRIPT_DIR/.."

SQL_FILES=$(find "$ROOT/crates" \
    \( -name 'target' -o -name 'target-wt' \) -type d -prune \
    -o -name '*.sql' -type f -print \
    | sort)

if [ -z "$SQL_FILES" ]; then
    echo "no SQL files found"
    exit 0
fi

# The DQS guard below needs sqlite3.Connection.setconfig (Python 3.12+). Run the
# first interpreter that has it: KHIVE_SQL_LINT_PYTHON, then versioned names, then
# python3. When none has it, python3 runs and the lint fails closed with its message.
PYTHON=python3
for candidate in "${KHIVE_SQL_LINT_PYTHON:-}" python3.14 python3.13 python3.12 python3; do
    if [ -n "$candidate" ] && command -v "$candidate" >/dev/null 2>&1 \
        && "$candidate" -c 'import sqlite3, sys; sys.exit(0 if hasattr(sqlite3.Connection, "setconfig") else 1)' \
            >/dev/null 2>&1; then
        PYTHON=$candidate
        break
    fi
done

"$PYTHON" - "$SQL_FILES" "$ROOT" <<'PY'
import os
import re
import sqlite3
import sys

files = sys.argv[1].split("\n") if len(sys.argv) > 1 else []
files = [f for f in files if f.strip()]
failed = 0

def lint_connection():
    """Create a lint database that never treats unknown quoted names as strings."""
    missing = []
    if not callable(getattr(sqlite3.Connection, "setconfig", None)):
        missing.append("Connection.setconfig")
    for name in ("SQLITE_DBCONFIG_DQS_DML", "SQLITE_DBCONFIG_DQS_DDL"):
        if not hasattr(sqlite3, name):
            missing.append(name)
    if missing:
        raise SystemExit(
            "SQL lint: Python 3.12+ with SQLite DQS configuration support is required; "
            "missing " + ", ".join(missing)
        )
    con = sqlite3.connect(":memory:")
    try:
        con.setconfig(sqlite3.SQLITE_DBCONFIG_DQS_DML, False)
        con.setconfig(sqlite3.SQLITE_DBCONFIG_DQS_DDL, False)
    except (AttributeError, sqlite3.Error, ValueError) as error:
        con.close()
        raise SystemExit(f"SQL lint: cannot disable SQLite DQS: {error}") from error
    return con

def sql_parameter_surface(sql):
    """Hide quoted text and comments from bind checks, never preparation."""
    surface = list(sql)
    at = 0
    while at < len(sql):
        if sql.startswith("--", at):
            end = sql.find("\n", at + 2)
            if end < 0:
                end = len(sql)
        elif sql.startswith("/*", at):
            end = sql.find("*/", at + 2)
            end = len(sql) if end < 0 else end + 2
        elif sql[at] in "'\"`[":
            quote = sql[at]
            closing = "]" if quote == "[" else quote
            end = at + 1
            while end < len(sql):
                if sql[end] == closing:
                    end += 1
                    if quote != "[" and end < len(sql) and sql[end] == closing:
                        end += 1
                        continue
                    break
                end += 1
        else:
            at += 1
            continue
        for index in range(at, end):
            if surface[index] not in "\r\n":
                surface[index] = " "
        at = end
    return "".join(surface)

def rust_tokens(source):
    """Read registration syntax without mistaking comments or strings for Rust."""
    tokens = []
    at = 0
    while at < len(source):
        if source[at].isspace():
            at += 1
        elif source.startswith("//", at):
            end = source.find("\n", at)
            at = len(source) if end < 0 else end
        elif source.startswith("/*", at):
            depth = 1
            at += 2
            while depth and at < len(source):
                if source.startswith("/*", at):
                    depth += 1
                    at += 2
                elif source.startswith("*/", at):
                    depth -= 1
                    at += 2
                else:
                    at += 1
            if depth:
                raise ValueError("unterminated Rust comment")
        elif raw := re.match(r'r(#+)?"', source[at:]):
            end_marker = '"' + (raw.group(1) or "")
            end = source.find(end_marker, at + raw.end())
            if end < 0:
                raise ValueError("unterminated Rust raw string")
            end += len(end_marker)
            tokens.append(source[at:end])
            at = end
        elif source[at] == '"':
            end = at + 1
            while end < len(source) and source[end] != '"':
                end += 2 if source[end] == "\\" else 1
            if end >= len(source):
                raise ValueError("unterminated Rust string")
            tokens.append(source[at:end + 1])
            at = end + 1
        elif char := re.match(r"'(?:\\(?:u\{[0-9A-Fa-f_]+\}|x[0-9A-Fa-f]{2}|.)|[^'\\])'", source[at:]):
            tokens.append(char.group())
            at += char.end()
        elif word := re.match(r"[A-Za-z_][A-Za-z_0-9]*", source[at:]):
            tokens.append(word.group())
            at += word.end()
        else:
            tokens.append(source[at])
            at += 1
    return tokens

def split_rust_fields(tokens):
    fields, field, stack = [], [], []
    pairs = {"(": ")", "[": "]", "{": "}"}
    for token in tokens:
        if token == "," and not stack:
            if field:
                fields.append(field)
                field = []
            continue
        if token in pairs:
            stack.append(pairs[token])
        elif token in pairs.values():
            if not stack or stack.pop() != token:
                raise ValueError("unbalanced Rust registration")
        field.append(token)
    if stack:
        raise ValueError("unbalanced Rust registration")
    if field:
        fields.append(field)
    return fields

def registered_migration_files(root):
    core_sql = os.path.join(root, "crates/khive-db/sql") + os.sep
    if not any(path.startswith(core_sql) for path in files):
        return set()
    source = os.path.join(root, "crates/khive-db/src/migrations.rs")
    with open(source) as fh:
        tokens = rust_tokens(fh.read())

    def constant(name):
        starts = [i for i in range(len(tokens) - 1)
                  if tokens[i:i + 2] == ["const", name]]
        if len(starts) != 1:
            raise ValueError(f"expected one migration constant {name}")
        at = starts[0] + 2
        end = tokens.index(";", at)
        equals = tokens.index("=", at, end)
        return tokens[equals + 1:end]

    registry = constant("MIGRATIONS")
    if registry[:2] != ["&", "["] or registry[-1:] != ["]"]:
        raise ValueError("MIGRATIONS must be a literal VersionedMigration array")
    registered = set()
    entries = split_rust_fields(registry[2:-1])
    if not entries:
        raise ValueError("MIGRATIONS must not be empty")
    for entry in entries:
        if entry[:2] != ["VersionedMigration", "{"] or entry[-1:] != ["}"]:
            raise ValueError("unrecognized VersionedMigration registration")
        fields = split_rust_fields(entry[2:-1])
        up = [field[2:] for field in fields if field[:2] == ["up", ":"]]
        if len(up) != 1 or len(up[0]) != 1:
            raise ValueError("migration up must name one include constant")
        expression = constant(up[0][0])
        if (expression[:3] != ["include_str", "!", "("]
                or expression[-1:] != [")"] or len(expression) != 5):
            raise ValueError(f"unresolved migration include {up[0][0]}")
        target = re.fullmatch(r'"\.\./sql/([A-Za-z0-9_.-]+\.sql)"', expression[3])
        if not target:
            raise ValueError(f"migration include must name a file in ../sql: {up[0][0]}")
        path = os.path.join(root, "crates/khive-db/sql", target.group(1))
        if path not in files:
            raise ValueError(f"registered migration is missing: {path}")
        registered.add(path)
    return registered

try:
    registered = registered_migration_files(sys.argv[2])
except (OSError, ValueError) as error:
    print(f"SQL lint: cannot resolve registered migrations: {error}")
    sys.exit(1)

# ── Hygiene + format (file-local, every file) ──────────────────────────────
create_re = re.compile(r"^\s*CREATE\s+(VIRTUAL\s+)?TABLE\b", re.IGNORECASE)
for path in files:
    with open(path) as fh:
        sql = fh.read()
    for i, line in enumerate(sql.splitlines(), 1):
        if line.rstrip() != line:
            print(f"{path}:{i}: trailing whitespace")
            failed += 1
        if "\t" in line:
            print(f"{path}:{i}: tab character (use spaces)")
            failed += 1
    # A jammed single-line table has the opening `(`, a column comma, and the
    # closing `)` all on one physical line. Single-column one-liners are fine.
    for i, line in enumerate(sql.splitlines(), 1):
        if create_re.match(line) and "(" in line and ")" in line and "," in line.split("(", 1)[1]:
            print(f"{path}:{i}: jammed CREATE TABLE — put one column per line")
            failed += 1

# ── Execution ──────────────────────────────────────────────────────────────
# khive-db migrations replay as a chain; other directories own independent schemas.
def in_db_chain(path):
    return "/khive-db/sql/" in path.replace(os.sep, "/")

def chain_order(path):
    base = os.path.basename(path)
    if base == "schema.sql":
        return 0  # V1 baseline applies first
    m = re.match(r"(\d+)", base)
    return int(m.group(1)) if m else 10**9

# Classification is by content, not by path: the first keyword decides. A file that
# opens with CREATE/ALTER/DROP/PRAGMA/BEGIN declares schema; anything else is a
# statement that a caller binds and runs.
ddl_re = re.compile(r"\s*(CREATE|ALTER|DROP|PRAGMA|BEGIN|COMMIT)\b", re.IGNORECASE)

def is_ddl(path):
    with open(path) as fh:
        for line in fh:
            stripped = line.strip()
            if not stripped or stripped.startswith("--"):
                continue
            return bool(ddl_re.match(line))
    return True  # an empty file has nothing to prepare

ddl = {f for f in files if is_ddl(f)}
chain = sorted([f for f in files if f in registered or (in_db_chain(f) and f in ddl)],
               key=chain_order)
ddl_files = [f for f in files if f in ddl and f not in chain]
query_files = [f for f in files if f not in ddl and f not in registered]

# This guarded pack upgrade requires the pre-namespace table, not current DDL.
GTD_LEGACY_FIXTURE = """CREATE TABLE gtd_lifecycle_audit (
    note_id TEXT NOT NULL,
    from_state TEXT NOT NULL,
    to_state TEXT NOT NULL,
    note TEXT,
    at INTEGER NOT NULL
)
"""

def gtd_legacy_upgrade(root):
    directory = os.path.join(root, "crates/khive-pack-gtd/sql")
    paths = {role: os.path.join(directory, "task-lifecycle-audit-" + name + ".sql")
             for role, name in [("upgrade", "add-namespace"), ("table", "ddl"),
                                ("index", "note-index"), ("inspect", "table-info")]}
    if not any(path in files for path in paths.values()):
        return None
    for path in paths.values():
        if path not in files:
            raise ValueError(f"incomplete GTD upgrade bundle: missing {path}")
        if path not in ddl_files:
            raise ValueError(f"GTD upgrade bundle must contain pack DDL: {path}")
    return paths

def validate_gtd_legacy_upgrade(paths):
    fixture = "gtd_lifecycle_audit_without_namespace"
    if any(line.rstrip() != line or "\t" in line for line in GTD_LEGACY_FIXTURE.splitlines()):
        raise ValueError(f"{fixture}: invalid fixture whitespace")
    statements = {}
    for role, path in paths.items():
        with open(path) as fh:
            statements[role] = fh.read()
    old_layout = [(at, name, kind, required, None, 0)
                  for at, (name, kind, required) in enumerate([
                      ("note_id", "TEXT", 1), ("from_state", "TEXT", 1),
                      ("to_state", "TEXT", 1), ("note", "TEXT", 0),
                      ("at", "INTEGER", 1)])]
    new_layout = old_layout + [(5, "namespace", "TEXT", 0, None, 0)]

    def expect_layout(con, sql, expected, context):
        actual = con.execute(sql).fetchall()
        if actual != expected:
            raise ValueError(f"{fixture}: {context}: unexpected table layout {actual!r}")

    con = sqlite3.connect(":memory:")
    try:
        con.execute(GTD_LEGACY_FIXTURE)
        expect_layout(con, "PRAGMA table_info(gtd_lifecycle_audit)", old_layout, "legacy fixture")
        con.execute(statements["table"])
        con.execute(statements["index"])
        expect_layout(con, statements["inspect"], old_layout, "before upgrade")
        con.execute("INSERT INTO gtd_lifecycle_audit VALUES (?, ?, ?, ?, ?)",
                    ("fixture-id", "inbox", "next", None, 7))
        # execute admits one statement; the actual ALTER runs once, without a skip.
        con.execute(statements["upgrade"])
        expect_layout(con, "PRAGMA table_info(gtd_lifecycle_audit)", new_layout, "after upgrade")
        if con.execute("SELECT * FROM gtd_lifecycle_audit").fetchall() != [
                ("fixture-id", "inbox", "next", None, 7, None)]:
            raise ValueError(f"{fixture}: upgrade changed the retained audit row")
    finally:
        con.close()

    con = sqlite3.connect(":memory:")
    try:
        con.execute(statements["table"])
        con.execute(statements["index"])
        expect_layout(con, statements["inspect"], new_layout, "canonical schema")
    finally:
        con.close()

try:
    gtd_upgrade = gtd_legacy_upgrade(sys.argv[2])
except ValueError as error:
    print(f"SQL lint: cannot resolve GTD legacy upgrade: {error}")
    sys.exit(1)
legacy_upgrade_files = {gtd_upgrade["upgrade"]} if gtd_upgrade else set()
ddl_files = [path for path in ddl_files if path not in legacy_upgrade_files]
populations = [set(chain), set(ddl_files), set(query_files), legacy_upgrade_files]
if (set.union(*populations) != set(files)
        or sum(map(len, populations)) != len(files)):
    print("SQL lint: validation populations must cover every file exactly once")
    sys.exit(1)
if gtd_upgrade:
    try:
        validate_gtd_legacy_upgrade(gtd_upgrade)
    except (OSError, sqlite3.Error, sqlite3.Warning, ValueError) as error:
        print(f"{gtd_upgrade['upgrade']}: FAILED legacy fixture gtd_lifecycle_audit_without_namespace: {error}")
        failed += 1

# Replay the migration chain cumulatively in one database so a forward migration
# (e.g. ALTER TABLE / CREATE INDEX on a baseline table) sees prior schema.
con = lint_connection()
try:
    for path in chain:
        with open(path) as fh:
            sql = fh.read()
        try:
            con.executescript(sql)
        except sqlite3.Error as e:
            print(f"{path}: FAILED to apply on the migration chain: {e}")
            failed += 1
finally:
    con.close()

# Related fragments share a database only within their own directory. A pack's
# sorted table/index fragments see prior DDL; unrelated packs never see each other.
# The tool-pack trigger requires core entities; its own fragments create grants.
fragment_groups = {}
for path in ddl_files:
    fragment_groups.setdefault(os.path.dirname(path), []).append(path)
for directory in sorted(fragment_groups):
    con = lint_connection()
    try:
        fixtures = []
        if directory.replace(os.sep, "/").endswith(
            ("/khive-pack-tool/sql", "/khive-pack-git/sql")
        ):
            fixtures = chain
        for path in fixtures + sorted(fragment_groups[directory]):
            with open(path) as fh:
                sql = fh.read()
            try:
                con.executescript(sql)
            except sqlite3.Error as e:
                print(f"{path}: FAILED to load: {e}")
                failed += 1
    finally:
        con.close()

# ── Preparation (query files) ──────────────────────────────────────────────
# Statements extracted out of Rust are queries, not DDL. Executing one is both
# wrong: unbound parameters act as NULL and can mutate rows or fail constraints.
# PREPARE instead against a database holding every table this tree declares.
if query_files:
    con = lint_connection()
    try:
        for path in chain:
            with open(path) as fh:
                try:
                    con.executescript(fh.read())
                except sqlite3.Error:
                    pass  # already reported by the chain replay above
        for path in ddl_files:
            with open(path) as fh:
                try:
                    con.executescript(fh.read())
                except sqlite3.Error:
                    pass  # already reported by its own directory group
        # The population control: if the schema did not come up, every query below
        # fails for one shared reason and the run reads like 300 broken files.
        tables = con.execute(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'"
        ).fetchone()[0]
        if tables == 0:
            print("SQL lint: no tables built, so no query file can be prepared")
            failed += 1
        else:
            for path in query_files:
                with open(path) as fh:
                    sql = fh.read()
                if sql.count(";") > 1 or (sql.count(";") == 1 and not sql.rstrip().endswith(";")):
                    print(f"{path}: more than one statement — one statement per file")
                    failed += 1
                    continue
                parameter_sql = sql_parameter_surface(sql)
                binds = re.findall(r"\?(\d+)", parameter_sql)
                highest = max((int(b) for b in binds), default=0)
                if binds and sorted({int(b) for b in binds}) != list(range(1, highest + 1)):
                    print(f"{path}: positional binds must run 1..N with no gaps")
                    failed += 1
                    continue
                if (re.search(r"\?(?!\d)", parameter_sql)
                        or re.search(r"[:@$][A-Za-z_]", parameter_sql)):
                    print(f"{path}: use numbered binds (?1, ?2), not anonymous or named ones")
                    failed += 1
                    continue
                try:
                    con.execute("EXPLAIN " + sql.rstrip().rstrip(";"), [None] * highest)
                except sqlite3.Error as e:
                    print(f"{path}: FAILED to prepare: {e}")
                    failed += 1
    finally:
        con.close()

if failed:
    print(f"\nSQL lint: {failed} issue(s)")
    sys.exit(1)
print(f"SQL lint: {len(files)} file(s) OK "
      f"({len(query_files)} prepared, {len(files) - len(query_files)} executed)")
PY

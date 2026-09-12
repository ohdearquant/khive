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
#   1b. preparation — a QUERY file (one that does not start with DDL) is prepared,
#                   not executed, against a database carrying the migration chain
#                   plus every DDL fragment in the tree. Preparation is what
#                   resolves table and column names, so a typo in either fails
#                   here rather than in a test run. Positional binds are filled
#                   with NULL and the statement runs under EXPLAIN, so nothing is
#                   evaluated and no row is touched. A file is classified by its
#                   first keyword, not by its name or its directory.
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

python3 - "$SQL_FILES" <<'PY'
import os
import re
import sqlite3
import sys

files = sys.argv[1].split("\n") if len(sys.argv) > 1 else []
files = [f for f in files if f.strip()]
failed = 0

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

chain = sorted([f for f in files if in_db_chain(f)], key=chain_order)
others = [f for f in files if not in_db_chain(f)]
ddl_files = [f for f in others if is_ddl(f)]
query_files = [f for f in others if not is_ddl(f)]

# Replay the migration chain cumulatively in one database so a forward migration
# (e.g. ALTER TABLE / CREATE INDEX on a baseline table) sees prior schema.
con = sqlite3.connect(":memory:")
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
# A directory with one file preserves standalone validation.
fragment_groups = {}
for path in ddl_files:
    fragment_groups.setdefault(os.path.dirname(path), []).append(path)
for directory in sorted(fragment_groups):
    con = sqlite3.connect(":memory:")
    try:
        for path in sorted(fragment_groups[directory]):
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
# wrong and impossible (executescript refuses bound parameters), so they are
# PREPARED instead, against a database holding every table this tree declares.
if query_files:
    con = sqlite3.connect(":memory:")
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
                binds = re.findall(r"\?(\d+)", sql)
                highest = max((int(b) for b in binds), default=0)
                if binds and sorted({int(b) for b in binds}) != list(range(1, highest + 1)):
                    print(f"{path}: positional binds must run 1..N with no gaps")
                    failed += 1
                    continue
                if re.search(r"\?(?!\d)", sql) or re.search(r"[:@$][A-Za-z_]", sql):
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

#!/bin/sh
# Lint SQL DDL files: execution validation + hygiene + format.
#
# sqlfluff cannot parse the fts5/vec0 virtual-table extension syntax these files
# use (and has no working auto-formatter for it), so the checks are:
#   1. execution  — files must load cleanly into in-memory SQLite (fts5 is built
#                   into the stdlib sqlite3 module). Migration files under
#                   crates/khive-db/sql/ form a forward chain: schema.sql is the
#                   V1 baseline and later NNN-<name>.sql migrations ALTER/extend
#                   it, so they are replayed cumulatively in version order on one
#                   database. Any other *.sql file is an independent fragment and
#                   loads into its own fresh database.
#   2. hygiene    — no trailing whitespace, no tabs.
#   3. format     — multi-column CREATE TABLE must be one column per line
#                   (catches comma-jammed single-line tables).
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$SCRIPT_DIR/.."

SQL_FILES=$(find "$ROOT/crates" -name '*.sql' -type f | sort)

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
# khive-db migration files replay as a chain; everything else loads standalone.
def in_db_chain(path):
    return "/khive-db/sql/" in path.replace(os.sep, "/")

def chain_order(path):
    base = os.path.basename(path)
    if base == "schema.sql":
        return 0  # V1 baseline applies first
    m = re.match(r"(\d+)", base)
    return int(m.group(1)) if m else 10**9

chain = sorted([f for f in files if in_db_chain(f)], key=chain_order)
others = [f for f in files if not in_db_chain(f)]

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

# Independent fragments load into a fresh database each.
for path in others:
    with open(path) as fh:
        sql = fh.read()
    con = sqlite3.connect(":memory:")
    try:
        con.executescript(sql)
    except sqlite3.Error as e:
        print(f"{path}: FAILED to load: {e}")
        failed += 1
    finally:
        con.close()

if failed:
    print(f"\nSQL lint: {failed} issue(s)")
    sys.exit(1)
print(f"SQL lint: {len(files)} file(s) OK")
PY

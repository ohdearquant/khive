#!/bin/sh
# Lint SQL DDL files: execution validation + basic hygiene.
#
# sqlfluff cannot parse the fts5/vec0 virtual-table extension syntax these files
# use, so the meaningful check is execution: every SQL file must load cleanly into
# an in-memory SQLite database (fts5 is built into the stdlib sqlite3 module).
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$SCRIPT_DIR/.."

SQL_FILES=$(find "$ROOT/crates" -name '*.sql' -type f | sort)

if [ -z "$SQL_FILES" ]; then
    echo "no SQL files found"
    exit 0
fi

python3 - "$SQL_FILES" <<'PY'
import sqlite3
import sys

files = sys.argv[1].split("\n") if len(sys.argv) > 1 else []
files = [f for f in files if f.strip()]
failed = 0

for path in files:
    with open(path) as fh:
        sql = fh.read()

    # Hygiene: no trailing whitespace, no tabs.
    for i, line in enumerate(sql.splitlines(), 1):
        if line.rstrip() != line:
            print(f"{path}:{i}: trailing whitespace")
            failed += 1
        if "\t" in line:
            print(f"{path}:{i}: tab character (use spaces)")
            failed += 1

    # Execution: must load into a fresh in-memory SQLite database.
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

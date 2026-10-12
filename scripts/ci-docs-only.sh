#!/usr/bin/env bash
# ci-docs-only.sh <base> <head>
#
# Prints "true" when the change from <base> to <head> may run CI's documentation lane only, and "false"
# when it needs the full suite. The reasons go to stderr.
#
# The documentation lane is allowed when every path that differs is a Markdown file, no non-Markdown file
# at <head> names that file's base name on a line that does not begin with a comment marker, and the file
# is not one whose verb and pack count claims the smoke tests check against the live registry.
#
# A Markdown file named by code may be read by it (include_str!, a test fixture path, a crate readme), so
# a change to it runs the full suite. A comment marker is // or /* (not the doc comments ///, //!, /** and
# /*!, whose examples rustdoc compiles and runs), <!--, or #, * or -- followed by a space or the end of the
# line; a Rust attribute (#[ or #!), a deref (*x) or a command-line flag (--flag) is code.
#
# A file read only through a directory pattern is not found by name. The count scanner reads that way and
# needs a built binary, so tests/documented_verb_counts.py --published-markdown answers for it and a file
# it reads runs the full suite. The other readers of that kind (the ADR reference and status lints, the
# marketplace validator, deno fmt, and the repository-wide identifier ban in tests/khive-contract) run in
# the documentation lane itself. A dot-named file runs the full suite: the scanner's globs match it but
# its path predicate does not.
#
# Renames are listed as a deletion plus an addition, so a source file renamed to .md is still seen.
# Every failure prints "false": the classifier can only fail toward running the full suite.
set -uo pipefail

full() {
    printf 'full suite: %s\n' "$1" >&2
    echo false
    exit 0
}

if [ "$#" -ne 2 ]; then
    echo "usage: $0 <base> <head>" >&2
    exit 2
fi
base=$1
head=$2
scanner=$(cd "$(dirname "$0")/.." && pwd)/tests/documented_verb_counts.py || full "cannot resolve the repository root"

list=$(mktemp) || full "mktemp failed"
trap 'rm -f "$list"' EXIT
git diff --no-renames --name-only -z "$base" "$head" -- > "$list" || full "git diff $base $head failed"
[ -s "$list" ] || full "no changed paths between $base and $head"

count=0
while IFS= read -r -d '' path; do
    count=$((count + 1))
    case "$path" in
        *.md) ;;
        *) full "not Markdown: $path" ;;
    esac
    name=${path##*/}
    case "$name" in
        .*) full "$path is dot-named" ;;
    esac
    matches=$(git grep -I -h -F -e "$name" "$head" -- ':(exclude)*.md')
    rc=$?
    if [ "$rc" -gt 1 ]; then
        full "git grep for $name failed (rc $rc)"
    fi
    if [ "$rc" -eq 0 ]; then
        # Here-strings, not pipes: under pipefail an early-exiting grep -q would fail its writer and read as
        # "not named", the unsafe direction.
        code=$(grep -v -E '^[[:space:]]*(//([^/!]|$)|/\*([^*!]|$)|<!--|(#|\*|--)([[:space:]]|$))' <<< "$matches")
        [ "$?" -le 1 ] || full "comment filter failed for $name"
        if grep -q -F -e "$name" <<< "$code"; then
            full "$path is named by a non-Markdown file"
        fi
    fi
    python3 "$scanner" --published-markdown "$path" < /dev/null
    rc=$?
    case $rc in
        0) full "$path carries verb or pack counts the smoke tests check against the live registry" ;;
        1) ;;
        *) full "count scanner failed for $path (rc $rc)" ;;
    esac
    printf 'documentation only: %s\n' "$path" >&2
done < "$list"

[ "$count" -gt 0 ] || full "no paths read from the diff"
echo true

#!/usr/bin/env bash
# ci-docs-only.sh <base> <head>
#
# Prints "true" when the change from <base> to <head> may run CI's documentation lane only, and "false"
# when it needs the full suite. The reasons go to stderr.
#
# The documentation lane is allowed when every path that differs is a Markdown file, no non-Markdown file
# at <head> names that file's base name on any line, and the file is not one whose verb and pack count
# claims the smoke tests check against the live registry.
#
# A Markdown file named by code may be read by it (include_str!, a test fixture path, a crate readme), so
# a change to it runs the full suite. A name inside a comment counts too. A line read on its own cannot
# tell a comment from code in every language: rustdoc runs the examples in doc comments, rustc reads an
# attribute written `# [doc = ...]`, code can follow a closed /* */ on the same line, and a string literal
# can hold a line that starts like a comment. Reading such a line as a comment would skip a job that
# reads the file, so a file cited only in comments runs the full suite as well.
#
# The name search reads every regular file, binary ones included. git grep does not read a symlink's
# target or a submodule's files, so a symlink whose target holds the name runs the full suite, and so does
# any change while <head> has a submodule.
#
# A file read only through a directory pattern is not found by name. The count scanner reads that way and
# needs a built binary, so tests/documented_verb_counts.py --published-markdown answers for it and a file
# it reads runs the full suite. The other readers of that kind (the ADR reference and status lints, the
# marketplace validator, deno fmt, the repository-wide identifier ban in tests/khive-contract, and the
# shipping pack-set copy check) run in the documentation lane itself.
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
targets=$(mktemp) || full "mktemp failed"
trap 'rm -f "$list" "$targets"' EXIT
git diff --no-renames --name-only -z "$base" "$head" -- > "$list" || full "git diff $base $head failed"
[ -s "$list" ] || full "no changed paths between $base and $head"

entries=$(git ls-tree -r --format='%(objectmode) %(objectname)' "$head") || full "git ls-tree $head failed"
[ -n "$entries" ] || full "no files at $head"
while read -r mode oid; do
    case $mode in
        120000)
            git cat-file blob "$oid" >> "$targets" || full "cannot read symlink $oid"
            echo >> "$targets"
            ;;
        160000) full "$head has a submodule, whose files are not searched" ;;
    esac
done <<< "$entries"

count=0
while IFS= read -r -d '' path; do
    count=$((count + 1))
    case "$path" in
        *.md) ;;
        *) full "not Markdown: $path" ;;
    esac
    name=${path##*/}
    # git grep's own status, not a pipe: under pipefail an early-exiting reader would fail its writer and
    # read as "not named", the unsafe direction.
    git grep -q -F -e "$name" "$head" -- ':(exclude)*.md'
    rc=$?
    case $rc in
        0) full "$path is named by a non-Markdown file" ;;
        1) ;;
        *) full "git grep for $name failed (rc $rc)" ;;
    esac
    grep -a -q -F -e "$name" "$targets"
    rc=$?
    case $rc in
        0) full "$path is named by a symlink's target" ;;
        1) ;;
        *) full "symlink target search for $name failed (rc $rc)" ;;
    esac
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

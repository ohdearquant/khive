#!/usr/bin/env bash
# ci-docs-only.sh <base> <head> [<path>...]
#
# Prints "true" when the change from <base> to <head> may run CI's documentation lane only, and "false"
# when it needs the full suite. The reasons go to stderr. Given paths, it judges only those, and each must be
# a path the change touches: scripts/ci-test-scope.py admits the Markdown files of a mixed change this way, so
# the two classifiers share one rule.
#
# The documentation lane is allowed when every path that differs is a Markdown file, no non-Markdown file
# at <head> names that file's base name on any line, except listed inert documentation data, and the file is not one whose verb and pack count
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
# A base name that more than one file at <head> carries (README.md is the common case: every crate and the ADR
# index have one) is searched by what can reach this file rather than by the bare name, which any other file of
# that name also matches: the parent directory and the name as one path ("adr/README.md"), the parent directory
# and the name as separate literals on one line (a joined path such as `"adr" / "README.md"`), and the bare name
# in files inside the file's own directory, where a relative reference needs no directory. A symlink inside that
# directory is read the same way. A reference that reaches the file another way, such as a path assembled from
# a variable, is not found, like the directory-pattern readers above.
#
# Renames are listed as a deletion plus an addition, so a source file renamed to .md is still seen.
# Every failure prints "false": the classifier can only fail toward running the full suite.
set -uo pipefail

full() {
    printf 'full suite: %s\n' "$1" >&2
    echo false
    exit 0
}

if [ "$#" -lt 2 ]; then
    echo "usage: $0 <base> <head> [<path>...]" >&2
    exit 2
fi
base=$1
head=$2
shift 2
scanner=$(cd "$(dirname "$0")/.." && pwd)/tests/documented_verb_counts.py || full "cannot resolve the repository root"

list=$(mktemp) || full "mktemp failed"
targets=$(mktemp) || full "mktemp failed"
inert=$(mktemp) || full "mktemp failed"
names=$(mktemp) || full "mktemp failed"
links=$(mktemp) || full "mktemp failed"
trap 'rm -f "$list" "$targets" "$inert" "$names" "$links"' EXIT
git diff --no-renames --name-only -z "$base" "$head" -- > "$list" || full "git diff $base $head failed"
[ -s "$list" ] || full "no changed paths between $base and $head"

entries=$(git ls-tree -r --format='%(objectmode) %(objectname) %(path)' "$head") || full "git ls-tree $head failed"
[ -n "$entries" ] || full "no files at $head"
git ls-tree -r -z --name-only "$head" > "$names" || full "git ls-tree $head failed"
while read -r mode oid entry_path; do
    case $mode in
        120000)
            git cat-file blob "$oid" >> "$targets" || full "cannot read symlink $oid"
            echo >> "$targets"
            # The link's own path and its target, one line, for the shared-name rule's same-directory read.
            printf '%s\t' "$entry_path" >> "$links"
            git cat-file blob "$oid" >> "$links" || full "cannot read symlink $oid"
            echo >> "$links"
            ;;
        160000) full "$head has a submodule, whose files are not searched" ;;
    esac
done <<< "$entries"

# Read the checked-out classifier's policy once; an unreadable or malformed policy cannot narrow CI.
inert_data=$(dirname "$0")/ci-inert-docs-data.txt
python3 -P - "$inert_data" > "$inert" <<'PY_INERT'
import pathlib
import re
import sys

try:
    seen = set()
    for number, line in enumerate(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8").splitlines(), 1):
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        path, marker, reason = line.partition("#")
        path = path.strip()
        parts = path.split("/")
        if (not marker or not reason.strip() or len(parts) < 2 or parts[0] != "docs"
                or any(part in ("", ".", "..") for part in parts)
                or not re.fullmatch(r"[A-Za-z0-9_./-]+", path)
                or pathlib.PurePosixPath(path).suffix not in (".tsv", ".csv", ".txt")
                or path in seen or "\0" in line):
            raise ValueError(f"invalid inert data entry at line {number}")
        seen.add(path)
        sys.stdout.buffer.write(path.encode("utf-8") + b"\0")
except (OSError, UnicodeError, ValueError) as error:
    print(error, file=sys.stderr)
    sys.exit(1)
PY_INERT
[ "$?" -eq 0 ] || full "cannot read or parse inert documentation data list"
name_paths=(':(exclude)*.md')
# Readers that run in the documentation lane itself, so a Markdown file they name needs no other job: the ADR
# reference lint (scripts/ci.sh adr-lint, the Docs lint job; it also writes fixtures at docs/adr/README.md) and
# the count scanner's own test, which names docs/adr/README.md only to assert the file is tracked. The scanner's
# verdict on each changed path is asked below.
# The classifier itself names paths only in its comments and reads none of them, and a change to it already runs
# the full suite.
for reader in scripts/lint-adr-refs.sh tests/test_documented_verb_counts.py scripts/ci-docs-only.sh; do
    name_paths+=(":(top,exclude,literal)$reader")
done
# Files that name one Markdown path only as a fixture they write inside a temporary repository and never read the
# tracked file. Each "reader:path" pair exempts that reader for that one path; it still counts for every other path.
fixture_pairs=(
    # The classifier's own tests build an ADR index in each fixture repository.
    "scripts/tests/test_ci_workflows.py:docs/adr/README.md"
)
while IFS= read -r -d '' inert_path; do
    name_paths+=(":(top,exclude,literal)$inert_path")
done < "$inert"

paths=()
while IFS= read -r -d '' path; do
    paths+=("$path")
done < "$list"
if [ "$#" -gt 0 ]; then
    selected=()
    for wanted in "$@"; do
        found=no
        for path in "${paths[@]}"; do
            [ "$path" = "$wanted" ] && found=yes
        done
        [ "$found" = yes ] || full "$wanted is not a path changed between $base and $head"
        selected+=("$wanted")
    done
    paths=("${selected[@]}")
fi

count=0
for path in "${paths[@]}"; do
    count=$((count + 1))
    case "$path" in
        *.md) ;;
        *) full "not Markdown: $path" ;;
    esac
    grep_paths=("${name_paths[@]}")
    for pair in "${fixture_pairs[@]}"; do
        [ "${pair#*:}" = "$path" ] && grep_paths+=(":(top,exclude,literal)${pair%%:*}")
    done
    name=${path##*/}
    dir=""
    case $path in */*) dir=${path%/*} ;; esac
    # How many other files at <head> carry this base name. awk reads the whole list before it prints, so the
    # count is never cut short; a failed read refuses rather than reading as zero.
    others=$(tr '\0' '\n' < "$names" | awk -v n="$name" -v p="$path" -F/ '$NF == n && $0 != p { c++ } END { print c + 0 }') \
        || full "cannot count the files named $name"
    [[ "$others" =~ ^[0-9]+$ ]] || full "cannot count the files named $name"
    if [ -z "$dir" ] || [ "$others" -eq 0 ]; then
        # git grep's own status, not a pipe: under pipefail an early-exiting reader would fail its writer and
        # read as "not named", the unsafe direction.
        git grep -q -F -e "$name" "$head" -- "${grep_paths[@]}"
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
    else
        parent=${dir##*/}
        # Regular-expression forms of the two names, for the joined-path search.
        parent_re=$(printf '%s' "$parent" | sed 's/[][\\.*^$+?(){}|/]/\\&/g')
        name_re=$(printf '%s' "$name" | sed 's/[][\\.*^$+?(){}|/]/\\&/g')
        git grep -q -F -e "$parent/$name" "$head" -- "${grep_paths[@]}"
        rc=$?
        case $rc in
            0) full "$path is named as $parent/$name by a non-Markdown file" ;;
            1) ;;
            *) full "git grep for $parent/$name failed (rc $rc)" ;;
        esac
        git grep -q -E -e "[\"']${parent_re}[\"'].*[\"'/]${name_re}[\"']" "$head" -- "${grep_paths[@]}"
        rc=$?
        case $rc in
            0) full "$path is named as a joined $parent and $name by a non-Markdown file" ;;
            1) ;;
            *) full "git grep for a joined $parent and $name failed (rc $rc)" ;;
        esac
        git grep -q -F -e "$name" "$head" -- ":(top,literal)$dir/" "${grep_paths[@]}"
        rc=$?
        case $rc in
            0) full "$path is named by a non-Markdown file in $dir" ;;
            1) ;;
            *) full "git grep for $name under $dir failed (rc $rc)" ;;
        esac
        grep -a -q -F -e "$parent/$name" "$targets"
        rc=$?
        case $rc in
            0) full "$path is named by a symlink's target" ;;
            1) ;;
            *) full "symlink target search for $parent/$name failed (rc $rc)" ;;
        esac
        awk -F'\t' -v d="$dir/" -v n="$name" 'index($1, d) == 1 && index($2, n) { found = 1 } END { exit found ? 0 : 1 }' "$links"
        rc=$?
        case $rc in
            0) full "$path is named by a symlink in $dir" ;;
            1) ;;
            *) full "symlink search under $dir failed (rc $rc)" ;;
        esac
    fi
    python3 -P "$scanner" --published-markdown "$path" < /dev/null
    rc=$?
    case $rc in
        0) full "$path carries verb or pack counts the smoke tests check against the live registry" ;;
        1) ;;
        *) full "count scanner failed for $path (rc $rc)" ;;
    esac
    printf 'documentation only: %s\n' "$path" >&2
done

[ "$count" -gt 0 ] || full "no paths read from the diff"
echo true

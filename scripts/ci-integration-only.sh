#!/usr/bin/env bash
# ci-integration-only.sh <base> <head>
#
# Prints "true" when the change from <base> to <head> may skip the build and test jobs of its own
# pull-request run and rely on the integration-branch run that tests it together with the pull requests
# merged beside it, and "false" when its pull-request run needs the full suite. The reasons go to stderr.
# The workflow asks only for pull requests from this repository's own `codex/` branches; this script
# judges the paths alone.
#
# The integration lane is allowed when every path that differs is Rust crate source under crates/ or a
# Markdown file. A path that changes what the build or CI does keeps the full suite on the pull request
# itself, so a change to it is seen by the run that the change alters:
#   - anything outside crates/ that is not Markdown: workflows and local actions under .github/, CI and
#     lint scripts under scripts/ (scripts/ci.sh included), the Python package, the apps, the
#     repository-level tests and benchmarks, and the Makefile;
#   - every Cargo.toml and Cargo.lock, at any depth;
#   - cargo, nextest and toolchain configuration: crates/.cargo/, crates/.config/, rust-toolchain files,
#     and deny.toml, rustfmt.toml and clippy.toml files wherever they sit.
# The list is an allow-list: a path the rules above do not name runs the full suite.
#
# Renames are listed as a deletion plus an addition, so a file moved out of crates/ is seen at its new
# path. A submodule at <head> runs the full suite. Every failure prints "false": the classifier can only
# fail toward running the full suite.
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

list=$(mktemp) || full "mktemp failed"
trap 'rm -f "$list"' EXIT
git diff --no-renames --name-only -z "$base" "$head" -- > "$list" || full "git diff $base $head failed"
[ -s "$list" ] || full "no changed paths between $base and $head"

modes=$(git ls-tree -r --format='%(objectmode)' "$head") || full "git ls-tree $head failed"
[ -n "$modes" ] || full "no files at $head"
case $'\n'"$modes"$'\n' in
    *$'\n'160000$'\n'*) full "$head has a submodule" ;;
esac

count=0
while IFS= read -r -d '' path; do
    count=$((count + 1))
    name=${path##*/}
    case "$name" in
        Cargo.toml|Cargo.lock) full "manifest or lockfile: $path" ;;
        rust-toolchain|rust-toolchain.toml) full "toolchain file: $path" ;;
        deny.toml|rustfmt.toml|.rustfmt.toml|clippy.toml|.clippy.toml) full "lint or policy configuration: $path" ;;
    esac
    case "$path" in
        crates/.cargo/*|crates/.config/*) full "cargo or nextest configuration: $path" ;;
        *.md) printf 'integration lane: %s\n' "$path" >&2 ;;
        crates/*) printf 'integration lane: %s\n' "$path" >&2 ;;
        *) full "outside crate source: $path" ;;
    esac
done < "$list"

[ "$count" -gt 0 ] || full "no paths read from the diff"
echo true

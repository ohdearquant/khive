#!/bin/sh
# check-no-stubs.sh — enforce the "No stubs. Ever." contributor policy.
#
# Flags the stub/debug macros in shipping Rust source:
#   todo!()          — unfinished implementation
#   unimplemented!() — deliberate non-implementation (still a stub)
#   dbg!()           — debug print left in shipping code
#
# Scope: crates/*/src/**/*.rs only. Excludes tests/ benches/ examples/.
# Single-line `//` and `///` comments are stripped before matching, so a macro
# name mentioned in a comment does not trip the guard. Block comments and string
# literals are not parsed: this is a macro screen, not a Rust parser. Patterns
# that are not these three macros (panic!("todo"), Err(NotImplemented), empty
# bodies) are out of scope and sit below the reversibility floor anyway.
#
# Not flagged: #[allow(dead_code)] — the forward-deployed crates (khive-merge,
# khive-vcs) legitimately carry dead_code attributes; this guard does not touch
# that attribute.
#
# POSIX sh (dash-compatible): newline-delimited file iteration, no `read -d`.
set -e

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC_ROOT="$REPO_ROOT/crates"

HITS="$(
    find "$SRC_ROOT" -type f -name "*.rs" \
        -path "*/src/*" \
        ! -path "*/tests/*" \
        ! -path "*/benches/*" \
        ! -path "*/examples/*" \
    | while IFS= read -r f; do
        # Blank single-line // comments (sed never deletes lines, so grep -n
        # line numbers still match the original file), then match the macros
        # and re-attach the filename.
        sed 's://.*$::' "$f" \
            | grep -n -e 'todo!(' -e 'unimplemented!(' -e 'dbg!(' \
            | sed "s|^|$f:|" || true
    done
)"

if [ -z "$HITS" ]; then
    echo "No stub macros (todo!/unimplemented!/dbg!) found in shipping source."
    exit 0
fi

echo "STUB MACROS found in shipping source (todo!/unimplemented!/dbg!):"
echo "$HITS"
echo ""
echo "khive contributor policy: No stubs. Ever. Remove or implement the above before merging."
exit 1

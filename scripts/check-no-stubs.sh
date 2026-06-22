#!/bin/sh
# check-no-stubs.sh — enforce "No stubs. Ever." policy (khive CLAUDE.md)
#
# Greps shipping Rust source for stub/debug macros:
#   todo!()        — marks unfinished implementation
#   unimplemented!() — marks deliberate non-implementation (still a stub)
#   dbg!()         — debug print left in shipping code
#
# Scope: crates/*/src/**/*.rs only.
# Excluded: tests/ benches/ examples/ (all pruned by the find command).
# Not flagged: #[allow(dead_code)] — the forward-deployed crates (khive-merge,
# khive-vcs) legitimately carry dead_code attributes; this guard does not
# touch that attribute.
set -e

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC_ROOT="$REPO_ROOT/crates"

HITS="$(
    find "$SRC_ROOT" -type f -name "*.rs" \
        -path "*/src/*" \
        ! -path "*/tests/*" \
        ! -path "*/benches/*" \
        ! -path "*/examples/*" \
    | xargs grep -n "todo!(\|unimplemented!(\|dbg!(" 2>/dev/null || true
)"

if [ -z "$HITS" ]; then
    echo "No stub macros (todo!/unimplemented!/dbg!) found in shipping source."
    exit 0
fi

echo "STUB MACROS found in shipping source (todo!/unimplemented!/dbg!):"
echo "$HITS"
echo ""
echo "khive policy: No stubs. Ever. Remove or implement the above before merging."
exit 1

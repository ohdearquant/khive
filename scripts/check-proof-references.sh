#!/bin/sh
# check-proof-references.sh — validate PROOF CORRESPONDENCE namespace coverage
#
# For every `PROOF CORRESPONDENCE: khive.Dir.Module.theorem` comment in Rust
# source, asserts that proofs/Dir/Module.lean exists.
#
# Namespace format: khive.<Dir>.<Module>.<theorem>
# File mapping:     proofs/<Dir>/<Module>.lean
#
# Example: khive.Retrieval.BM25.idf_nonneg → proofs/Retrieval/BM25.lean
#
# Usage: ./scripts/check-proof-references.sh
#        Returns exit code 1 if any reference is missing a stub file.

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$SCRIPT_DIR/.."
CRATES_DIR="$ROOT/crates"
PROOFS_DIR="$ROOT/proofs"

missing=0

namespaces=$(grep -rh 'PROOF CORRESPONDENCE' "$CRATES_DIR" --include='*.rs' \
    | grep -oE 'khive\.[A-Za-z][A-Za-z0-9_]*\.[A-Za-z][A-Za-z0-9_]*\.[A-Za-z][A-Za-z0-9_]*' \
    | sort -u)

for namespace in $namespaces; do
    # khive.Retrieval.BM25.idf_nonneg
    # Strip 'khive.' prefix → Retrieval.BM25.idf_nonneg
    without_prefix="${namespace#khive.}"
    # Split on dots: dir=Retrieval, module=BM25, _theorem=idf_nonneg
    dir=$(echo "$without_prefix" | cut -d. -f1)
    module=$(echo "$without_prefix" | cut -d. -f2)
    lean_file="$PROOFS_DIR/$dir/$module.lean"
    if [ ! -f "$lean_file" ]; then
        echo "MISSING proof file: $lean_file (referenced by namespace $namespace)"
        missing=1
    fi
done

if [ "$missing" -eq 0 ]; then
    echo "Proof reference check: OK (all cited namespaces have stub files)"
fi

exit "$missing"

# PR #967 review fixes

## Implementation

- Added a namespace-agnostic runtime graph read for live incoming `annotates` edges by target ID, and routed `get(edge_id)` annotation discovery through it without changing visibility-scoped neighbor traversal.
- Chunked SQLite note batch hydration at 900 bind variables, merging all chunks before the handler restores annotation-neighbor order.
- Added integration regressions for a foreign edge and note created in `ns-a` then fetched from `ns-b`, and for an edge with 1,000 annotations.
- Replaced the prohibited edge-annotation fixture identifiers with neutral public names and content.
- Documented the `get` edge response's always-present `annotations` array and per-note `annotation_edge_id`.

## Verification

- Red phase: `get_edge_cross_namespace_includes_foreign_annotation` failed with zero annotations before the implementation change.
- `cargo test -p khive-pack-kg --test integration`: 239 passed.
- `cargo check --workspace`: passed.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed.
- `cargo fmt --all -- --check`: passed.
- `deno fmt docs/guide/api-reference.md`: passed.
- `git diff --check`: passed.

## Domain utility

`medium` — the composed testing/query-semantics domains reinforced contract-level boundary coverage; the repository's ADR-007 contract and existing 900-row graph chunking convention determined the implementation details.

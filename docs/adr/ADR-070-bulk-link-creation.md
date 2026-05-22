# ADR-070: Bulk Link Creation

**Status**: proposed\
**Date**: 2026-05-21\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-014 (KG Curation Operations), ADR-020 (Request DSL), ADR-031 (Pack-Extensible Edge Endpoints)\
**Does not reverse**: ADR-014 §rejection of dedicated `bulk_link` MCP tool

## Context

Creating N edges currently requires N separate `link()` calls or N operations inside a
`request` batch. For small N this is fine. For large N — importing a citation graph,
bulk-linking research papers to their authors — the overhead is significant: N round-trips
or a single batch that consumes most of the ADR-020 100-op cap.

The storage layer already supports atomic batch edge insertion via
`GraphStore::upsert_edges` (`crates/khive-storage/src/graph.rs:14`) backed by a
`BEGIN IMMEDIATE` loop in `crates/khive-db/src/stores/graph.rs:331`. The runtime does not
currently expose this path through any handler.

ADR-014 rejected a dedicated `bulk_link` MCP tool, favouring the generic `request` tool as the
composition mechanism. This ADR does not add a new MCP tool. Instead it extends the existing
`link` verb to accept an array of link entries alongside its current singleton form.

## Decision

Extend `link` to accept either singleton params (current shape) or a `links` array. The
`links` key's presence is the discriminator.

### Input shapes

**Singleton (unchanged)**:

```json
{ "source_id": "abc", "target_id": "def", "relation": "extends", "weight": 0.9 }
```

**Bulk**:

```json
{
  "links": [
    { "source_id": "abc", "target_id": "def", "relation": "extends", "weight": 0.9 },
    { "source_id": "ghi", "target_id": "jkl", "relation": "contains" }
  ],
  "namespace": "research",
  "atomic": true,
  "verbose": false
}
```

### Rust type additions

```rust
// crates/khive-pack-kg/src/handlers.rs

#[derive(Deserialize)]
struct BulkLinkEntry {
    source_id: String,
    target_id: String,
    relation: String,
    weight: Option<f64>,
    metadata: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct LinkParams {
    namespace: Option<String>,
    // singleton fields (all optional when `links` is present)
    source_id: Option<String>,
    target_id: Option<String>,
    relation: Option<String>,
    weight: Option<f64>,
    // bulk fields
    links: Option<Vec<BulkLinkEntry>>,
    atomic: Option<bool>,   // default true
    verbose: Option<bool>,
}
```

### Validation

1. All `source_id` and `target_id` values are resolved via `resolve_uuid_async` before any write.
2. Endpoint kind rules (`validate_edge_relation_endpoints`) are enforced for every entry.
3. All weights are clamped to `[0.0, 1.0]`.
4. Duplicate natural keys `(source, target, relation)` within the same request are rejected
   before hitting storage (checked in-process, not via DB round-trip).

### Atomicity

`atomic = true` (default): all entries are validated and built into `Vec<Edge>` before
`upsert_edges` is called. The `BEGIN IMMEDIATE` transaction in the DB layer either commits all
or rolls back all. On validation error, no edges are written.

`atomic = false` (opt-in): entries are written in one `upsert_edges` call but per-entry
storage errors (e.g. referential integrity) are tolerated. Returns per-entry success/failure
in the bulk response.

### Limit

Maximum **1000 entries** per bulk `link` call. This is separate from ADR-020's 100-op cap
on `request` batches. A single `link(links=[...])` inside `request` counts as one of the 100
ops but can carry up to 1000 edges.

### Return shapes

**Singleton**: unchanged — returns the single `Edge` JSON as before.

**Bulk**:

```json
{
  "attempted": 3,
  "created": 3,
  "skipped": 0,
  "failed": 0,
  "edges": [ { ...edge... }, { ...edge... }, { ...edge... } ],
  "errors": []
}
```

`BatchWriteSummary` from `crates/khive-db/src/stores/graph.rs:331` maps to `attempted`,
`created` (= `affected`), and `failed`. `skipped` counts natural-duplicate entries coalesced
before the DB call.

## Consequences

### Positive

- Closes the N-call overhead for bulk edge creation without adding a new MCP tool.
- Reuses `GraphStore::upsert_edges` which is already transactional and tested.
- Singleton callers are unaffected; the new path activates only when `links` is present.

### Negative

- Handler logic gains a discriminating branch; the two paths must stay consistent.
  A shared `build_edge` helper can keep per-entry validation DRY.
- `atomic = false` semantics are more complex to document and test.
  Consider deferring the non-atomic path and starting with `atomic = true` only.

### Tests required

- Singleton call returns a single edge (backward-compat regression test).
- Valid bulk insert: all edges created, summary correct.
- Invalid endpoint in atomic mode: zero edges written, error returned.
- Duplicate natural key in same request: rejected before storage call.
- Weight clamped to [0.0, 1.0] in bulk entries.
- Limit: 1001 entries rejected before any validation.
- `atomic = false`: partial success returns per-entry error list.
- Bulk `link` inside a `request` batch counts as one of the 100 ops.

## References

- ADR-014: KG Curation Operations — rejects dedicated `bulk_link` MCP tool
- ADR-020: Request DSL — `request` is the composition layer; bulk link is one op inside it
- ADR-031: Pack-Extensible Edge Endpoints — endpoint kind enforcement applies per-entry
- `crates/khive-storage/src/graph.rs:14`: `GraphStore::upsert_edges` trait method
- `crates/khive-db/src/stores/graph.rs:331`: SQLite `upsert_edges` implementation
- `crates/khive-runtime/src/operations.rs:400`: existing `link` + endpoint validation

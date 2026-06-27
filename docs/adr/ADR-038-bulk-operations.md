# ADR-038: Bulk Operations

**Status**: accepted
**Date**: 2026-05-23
**Authors**: Ocean, lambda:khive
**Consolidates**: ADR-069 and ADR-070 — draft proposals, never published as standalone ADRs
**Depends on**: ADR-002 (Edge Ontology), ADR-014 (Curation Operations), ADR-016 (Request DSL), ADR-017 (Pack Standard), ADR-029 (SubstrateCoordinator)

## Context

Two related problems emerged as khive usage scaled.

**Bulk edge creation.** Creating N edges currently requires N separate `link()` calls or N
operations inside a `request` batch. For small N this is acceptable. For large N — importing a
citation graph, bulk-linking research papers to their authors, seeding a domain ontology — the
overhead is significant: N round trips or a single batch that consumes most of ADR-016's 100-op
cap. The storage layer already supports atomic batch edge insertion via
`GraphStore::upsert_edges`, backed by a `BEGIN IMMEDIATE` loop. No handler exposes this path.

ADR-014 rejected a dedicated `bulk_link` MCP tool in favour of the generic `request` tool as
the composition mechanism. This ADR respects that decision: it extends the existing `link` verb
rather than adding a new tool.

**Batch write conflicts.** The `request` MCP tool dispatches parallel batch operations via
`futures::future::join_all`. The SQLite writer connection is protected by a single `Mutex`, so
concurrent writes serialize at the storage layer, preventing data corruption. That serialization
does not prevent logical conflicts.

A parallel batch like:

```text
[update(kind="entity", id="abc", name="Foo"), update(kind="entity", id="abc", name="Bar")]
```

produces last-writer-wins behaviour determined by whichever future acquires the writer lock
first — an ordering that is non-deterministic from the caller's perspective. ADR-016 specifies
that batch failures do not roll back, which correctly describes independent errors but does not
address same-entity write conflicts within a batch. No prior ADR specifies a write-set preflight.

## Decision

### Part 1: Bulk Link Creation

Extend the `link` verb to accept either the existing singleton params or a `links: [...]` array.
The presence of the `links` key is the discriminator.

#### Input shapes

Singleton (unchanged):

```json
{ "source_id": "abc", "target_id": "def", "relation": "extends", "weight": 0.9 }
```

Bulk:

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

#### Rust type additions

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
#[serde(deny_unknown_fields)]
struct LinkParams {
    // singleton fields — all optional when `links` is present
    source_id: Option<String>,
    target_id: Option<String>,
    relation: Option<String>,
    weight: Option<f64>,
    metadata: Option<Value>,
    dependency_kind: Option<String>,
    verbose: Option<bool>,
    // bulk fields
    links: Option<Vec<BulkLinkEntry>>,
    atomic: Option<bool>,   // default true
}
```

#### Validation

Every entry in a bulk call undergoes the same validation as a singleton:

1. `source_id` and `target_id` are resolved via `resolve_uuid_async` before any write.
2. Endpoint kind rules (`validate_edge_relation_endpoints` per ADR-002) are enforced for every entry.
3. Weights are clamped to `[0.0, 1.0]`.
4. Duplicate natural keys `(source, target, relation)` within the same call are coalesced before
   storage and counted in `skipped`.

#### Atomicity modes

`atomic = true` (default): all entries are resolved and built into `Vec<LinkSpec>` before
`runtime.link_many` is called. The DB layer either commits the entire set or rolls back entirely.
If any entry fails validation before `link_many`, no edges are written.

`atomic = false` (opt-in): entries are attempted one by one through singleton `runtime.link`.
Validation/storage errors are collected in `errors`, successful entries commit individually, and
duplicate natural keys are counted in `skipped`.

#### Limit

Maximum 1000 entries per bulk `link` call. This limit is separate from ADR-016's 100-op cap on
`request` batches. A single `link(links=[...])` inside `request` counts as one of the 100 ops but
can carry up to 1000 edges.

#### Return shapes

Singleton: unchanged — returns the single `Edge` JSON as before.

Bulk:

```json
{
  "attempted": 3,
  "created": 3,
  "skipped": 0,
  "failed": 0,
  "edges": [ { "...edge..." }, { "...edge..." }, { "...edge..." } ],
  "errors": []
}
```

`BatchWriteSummary` from `crates/khive-db/src/stores/graph.rs` maps to `attempted`, `created`
(= `affected`), and `failed`. `skipped` counts natural-duplicate entries coalesced before the DB
call. When `verbose = false`, `edges` is omitted from the response.

---

### Part 2: Request Batch Conflict Detection

Add a write-set preflight step in `run_parsed` before `join_all`. If two operations in the same
parallel batch target the same write key, conflicting operations receive per-op conflict errors;
non-conflicting operations still dispatch. This preserves the ADR-016 envelope invariant:
`results.length == summary.total == input.ops.length`.

#### Write-set model

The shipped preflight uses static parser-side extraction in `khive-request`, not a
`PackRuntime::write_keys` trait method. Current key formats are parser-owned opaque strings:

```text
entity:<uuid>
edge-natural:<source_uuid>:<target_uuid>:<relation>
```

Known implementation gap: the intended stable error shape includes `conflict_ops`, and bulk
`link(links=[...])` should contribute every contained natural edge key. The current shipped
implementation omits `conflict_ops` and only extracts singleton `link(source_id, target_id,
relation)` keys. Keep those as code-side follow-ups; do not change this ADR to claim bulk
array conflict protection is complete.

#### Preflight algorithm

```rust
// crates/khive-mcp/src/server.rs (pseudocode)
fn preflight_conflict_check(
    ops: &[ParsedOp],
    registry: &VerbRegistry,
    default_ns: &str,
) -> Result<(), BatchConflictError> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut conflicts: Vec<(usize, usize, String)> = Vec::new();

    for (i, op) in ops.iter().enumerate() {
        let keys = registry.write_keys_for(
            &op.tool,
            &Value::Object(op.args.clone()),
            default_ns,
        );
        for key in keys.iter().flatten() {
            if let Some(&prior) = seen.get(key) {
                conflicts.push((prior, i, key.clone()));
            } else {
                seen.insert(key.clone(), i);
            }
        }
    }

    if conflicts.is_empty() { Ok(()) } else { Err(BatchConflictError { conflicts }) }
}
```

The preflight runs after parsing and before gate enforcement. If the check detects write-set
overlaps, it does NOT abort the entire batch. Instead it emits per-op conflict errors, preserving
the ADR-016 contract that `results.length == summary.total == input.ops.length`.

## Conflict semantics (ADR-016 alignment)

When the pre-dispatch conflict detector identifies overlapping write-sets in a
bulk operation:

- The request envelope still succeeds (`request.ok == true`).
- Each conflicting op returns `{ok: false, error: "conflict: writes overlap with
  op #<idx>", conflict_ops: [<idx1>, <idx2>, ...]}`.
- Non-conflicting ops execute normally.
- `results.length == summary.total == input.ops.length` (ADR-016 contract preserved).

If ordered dependency semantics are required, the caller uses top-level pipe-chain syntax
(ADR-016 `op1(...) | op2(...)`) which aborts the chain on first failure. Do not wrap pipe
chains in `[...]`; bracketed form is the parallel batch syntax.

#### Error shape (per conflicting op)

```json
{
  "ok": false,
  "error": "conflict: writes overlap with op #2",
  "conflict_ops": [2]
}
```

Each conflicting op receives its own `{ok: false}` entry. Non-conflicting ops receive their
normal result. The aggregate `summary` reflects the actual executed/failed counts.

#### Read/write classification

Read verbs (`recall`, `search`, `get`, `list`, `neighbors`, `traverse`, `query`) never conflict
with any other op and never produce write keys.

Write verbs (`create`, `update`, `delete`, `link`, `merge`) implement `write_keys` when the
target ID is statically determinable from params at parse time. Verbs whose target requires a
database lookup (for example, `update` by `name` rather than `id`) return `None` and rely on the
existing DB-level serialization.

Mixed batches (reads + writes) are allowed. Only write-write key collisions are flagged.

#### Sequential escape hatch

Callers that need dependent writes use the pipe-chain syntax (ADR-016 `|` operator). The
preflight does not introduce implicit sequencing into comma-separated batches. When a caller
genuinely needs "update entity A, then update entity A with A's new state," the chain form is
the right expression:

```text
update(kind="entity", id="abc", name="Foo") | update(kind="entity", id=$prev.id, description="...")
```

#### Unknown verb fallback

If a verb is not registered, or its `write_keys` returns `None`, the preflight treats the op as
non-conflicting and allows it through. This preserves forward compatibility: a new verb that does
not yet implement `write_keys` will not cause spurious batch rejections.

#### Interaction with bulk link

A bulk `link(links=[...])` is a single op. Its write keys are the natural edge keys for every
entry in the batch. If another op in the same `request` batch targets any of those same edge
natural keys, the preflight rejects the batch before either op executes.

## Rationale

### Why extend `link` rather than add a new verb?

ADR-014 explicitly rejected a dedicated `bulk_link` MCP tool. Extending `link` with a
`links` discriminator keeps the surface minimal: agents learn one verb shape, backends handle
both paths through a shared validation pipeline.

The discriminator (`links` key present) is unambiguous. The singleton shape has no `links`
field. There is no ambiguity when parsing either form.

### Why 1000 entries as the bulk limit?

1000 edges in a single DB transaction is well within SQLite's capability and observed import
use cases (citation graphs, paper-author relationships). The limit prevents accidental
unbounded payloads. If production usage requires more, it gets raised; the constant lives in
one place.

The limit is separate from ADR-016's 100-op batch cap. A `link` with 1000 entries is still
one of the 100 allowed ops.

### Why make `atomic = false` opt-in rather than the default?

Atomic writes are the safe default. Callers that do not think about atomicity get correct
all-or-nothing semantics. `atomic = false` is explicitly requested by callers that want partial
success semantics and are prepared to handle per-entry error lists.

### Why write-set preflight rather than storage-level conflict detection?

Conflict detection at the storage layer is invisible to the caller. A parallel batch with a
write-write conflict silently resolves via lock ordering (last-writer-wins). The caller receives
two `ok: true` results but only one value persisted — the result depends on scheduling,
not intent.

Preflight makes the conflict explicit before any mutation. The error identifies which ops
conflict and on which key. The caller can restructure the batch (use a chain for dependent
writes) before retrying.

### Why return per-op conflict errors rather than a single batch-level rejection?

ADR-016 requires `results.length == summary.total == input.ops.length`. A single batch-level
error that returns no per-op results violates this contract. By failing only the conflicting
ops and allowing non-conflicting ops to execute, the response envelope is always ADR-016
compliant. Callers that need atomic all-or-nothing semantics for dependent writes use the pipe
chain syntax (ADR-016 `|`), which aborts on first failure and is the explicit expression of
that intent.

### Why is the `write_keys` method opt-in (default `None`)?

Requiring all existing pack verbs to immediately implement `write_keys` would be a breaking
change to the trait. The default `None` treats unimplemented verbs as non-conflicting — they
rely on DB-level serialization as they do today. Packs opt in as they mature. Coverage can be
tracked per-verb.

## Alternatives Considered

| Alternative                                                          | Why rejected                                                                                                     |
| -------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------- |
| Dedicated `bulk_link` MCP tool                                       | ADR-014 rejected this; adds tool-count overhead without capability benefit.                                      |
| `link` accepts `links` as a top-level JSON array (no object wrapper) | No room for top-level `atomic`, `verbose`, `namespace` params. Object wrapper required.                          |
| `atomic = false` as default                                          | Unsafe default; callers who don't consider atomicity get surprising partial state.                               |
| Per-entry results even in atomic mode                                | Contradicts atomicity: if any entry fails, nothing is written; a per-entry result implies partial writes.        |
| Conflict detection at storage layer (silent)                         | Produces last-writer-wins, invisible to caller; no diagnostic output.                                            |
| Single batch-level conflict error (no per-op results)                | Violates ADR-016 contract: `results.length == summary.total == input.ops.length`; adopted per-op errors instead. |
| Optimistic conflict detection (allow batch, flag after)              | Creates partial state that must be reconciled; preflight avoids the problem entirely.                            |
| Make `write_keys` required (non-default)                             | Breaks all existing `PackRuntime` impls; adoption must be gradual.                                               |

## Consequences

### Positive

- Bulk edge insertion in a single round trip for import-scale workloads, without adding a new MCP tool.
- Reuses `GraphStore::upsert_edges` which is already transactional and tested.
- Singleton `link` callers are unaffected; the new path activates only when `links` is present.
- Write-write conflicts in parallel batches are surfaced before any mutation — no partial state.
- Error message identifies conflicting op indexes and the specific key — actionable diagnostic.
- Existing packs compile unchanged; conflict protection is opt-in at the pack level.

### Negative

- The `link` handler gains a discriminating branch. Both paths must stay consistent.
  A shared `build_edge` helper keeps per-entry validation DRY.
- `atomic = false` semantics are more complex to test and document.
- `write_keys` provides partial protection until all built-in write verbs implement it.
  Verbs that derive their target from a DB lookup cannot produce static keys and remain
  unprotected until a deeper preflight mechanism is designed.
- Preflight adds a linear scan over all ops on every parallel batch (O(N) in op count,
  O(E) in total write keys). For observed batch sizes (5-100 ops), this is negligible.

### Tests required

Bulk link:

- Singleton call returns a single edge (backward-compat regression test).
- Valid bulk insert: all edges created, summary fields correct.
- Invalid endpoint in atomic mode: zero edges written, error returned.
- Duplicate natural key in same call: rejected before storage call, skipped count incremented.
- Weight outside `[0.0, 1.0]` clamped, not rejected.
- Limit: 1001 entries rejected before any validation.
- `atomic = false`: partial success returns per-entry error list.
- Bulk `link` inside a `request` batch counts as one of the 100 ops.

Batch conflict detection:

- Conflicting `update`/`update` on the same entity ID is rejected before dispatch.
- Conflicting `merge`/`update` targeting the same entity is rejected.
- Conflicting `link`/`link` with the same natural edge key is rejected.
- Independent parallel writes (different entity IDs) pass through.
- Read + write on the same entity is allowed.
- Unknown verb (no `write_keys` implementation) does not block the batch.
- Structured error includes op indexes and the conflicting key.
- Bulk `link` write keys include all natural edge keys; conflict with a sibling op is detected.

## Open Questions

1. Should `atomic = false` be deferred to v2? The atomic path covers all known import
   use cases. Non-atomic semantics add implementation and documentation complexity.
2. When `write_keys` coverage is complete for built-in verbs, should `None` become an
   error rather than a pass-through? That would require versioning the `PackRuntime` trait.
3. Should the 1000-entry bulk limit be configurable in `RuntimeConfig`? Hardcoded for v1;
   revisit if operator deployments have different constraints.
4. Cross-backend write keys: ADR-029 (SubstrateCoordinator) routes ops to different backends.
   Write keys should include a backend prefix when a backend component becomes statically
   determinable. Not needed for v1 (single SQLite backend).

## References

- ADR-002: Edge Ontology — endpoint kind validation applied per-entry in bulk link.
- ADR-014: Curation Operations — `link` verb baseline; rejected dedicated `bulk_link` tool.
- ADR-016: Request DSL — batch semantics, 100-op cap, `|` chain syntax for sequential writes.
- ADR-017: Pack Standard — `PackRuntime` trait; `write_keys` method added as optional extension.
- ADR-029: SubstrateCoordinator — cross-backend write key namespacing (future).

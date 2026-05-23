# ADR-069: Request Batch Conflict Detection

**Status**: proposed\
**Date**: 2026-05-21\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-020 (Request DSL), ADR-028 (Request Parser Crate), ADR-025 (Pack Standard)

## Context

The `request` MCP tool dispatches parallel batch operations via `futures::future::join_all`
over `ParsedOp` values (`crates/khive-mcp/src/server.rs:232`). The SQLite writer connection is
protected by a single `Mutex` (`crates/khive-db/src/pool.rs:49`), so concurrent writes serialize
at the storage layer. This prevents data corruption but does not prevent logical conflicts.

A parallel batch like:

```
[update(kind="entity", id="abc", name="Foo"), update(kind="entity", id="abc", name="Bar")]
```

produces last-writer-wins behaviour determined by which future acquires the writer lock first —
an ordering that is non-deterministic from the caller's perspective.

ADR-020 §Negative states that batch failures do not roll back — a correct description of the
current behaviour for independent errors, but it does not address same-entity write conflicts.
No existing ADR specifies a write-set preflight.

## Decision

Add a **write-set preflight** step in `run_parsed` before `join_all`. If two operations in the
same parallel batch target the same write key, the entire batch is rejected before any dispatch.

### Write-set model

Each mutating verb reports a set of opaque conflict keys. The key format is:

```
entity:<namespace>:<uuid>
note:<namespace>:<uuid>
edge:<namespace>:<edge_id>
edge-natural:<namespace>:<source_uuid>:<target_uuid>:<relation>
```

Packs expose this metadata via a new optional method on `PackRuntime`:

```rust
// crates/khive-runtime/src/pack.rs
pub trait PackRuntime: Send + Sync {
    // ... existing methods ...

    /// Conflict keys this operation would write.
    ///
    /// Returns `None` if the verb is read-only or the keys cannot be determined
    /// statically from params (e.g. search, recall). The preflight treats `None`
    /// as non-conflicting.
    fn write_keys(
        &self,
        verb: &str,
        params: &Value,
        default_namespace: &str,
    ) -> Option<Vec<String>> {
        let _ = (verb, params, default_namespace);
        None
    }
}
```

Default implementation returns `None` — existing packs compile without changes.

### Preflight algorithm

```rust
// crates/khive-mcp/src/server.rs  (pseudocode)
fn preflight_conflict_check(
    ops: &[ParsedOp],
    registry: &VerbRegistry,
    default_ns: &str,
) -> Result<(), BatchConflictError> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut conflicts: Vec<(usize, usize, String)> = Vec::new();

    for (i, op) in ops.iter().enumerate() {
        let keys = registry.write_keys_for(&op.tool, &Value::Object(op.args.clone()), default_ns);
        for key in keys.iter().flatten() {
            if let Some(&prior) = seen.get(key) {
                conflicts.push((prior, i, key.clone()));
            } else {
                seen.insert(key.clone(), i);
            }
        }
    }

    if conflicts.is_empty() {
        Ok(())
    } else {
        Err(BatchConflictError { conflicts })
    }
}
```

### Error shape

```json
{
  "ok": false,
  "error": {
    "kind": "batch_conflict",
    "message": "parallel batch contains conflicting writes",
    "conflicts": [
      { "op_a": 0, "op_b": 2, "key": "entity:default:a1b2c3d4-..." }
    ]
  }
}
```

The response is returned at the batch level (not per-op) because no ops execute when a conflict
is detected.

### Read/write classification

- Read verbs (`recall`, `search`, `get`, `list`, `neighbors`, `traverse`, `query`) never
  conflict with any other op.
- Write verbs (`create`, `update`, `delete`, `link`, `merge`) report write keys.
- Mixed batches (reads + writes) are allowed; only write-write conflicts are flagged.

### Sequential escape hatch

Callers that need dependent writes should use pipe chains (ADR-020 §planned, issue #219).
The preflight does not introduce implicit sequencing into comma-separated batches.

### Unknown verb fallback

If a verb is not registered or its `write_keys` returns `None`, the preflight treats the op as
non-conflicting and allows it through. This preserves forward compatibility: a new verb that
does not yet implement `write_keys` will not cause spurious rejections.

## Consequences

### Positive

- Eliminates non-deterministic last-writer-wins in parallel batches.
- Error is returned before any storage mutation — no partial state to clean up.
- Opt-in for new packs; existing packs remain unaffected until they implement `write_keys`.

### Negative

- Packs must implement `write_keys` to benefit from protection. Until all built-in verbs
  implement it, partial coverage exists. Coverage can be tracked per-verb.
- Static key extraction requires that the verb's target ID is present in `params` at parse time.
  Verbs that derive their target from a database lookup (e.g. `update` by `name` rather than
  `id`) cannot produce a write key statically; they must return `None` and rely on DB serialization.

### Tests required

- Conflicting `update`/`update` on the same entity ID is rejected pre-dispatch.
- Conflicting `merge`/`update` targeting the same entity rejected.
- Conflicting `link`/`link` producing the same natural edge key rejected.
- Independent parallel writes (different entity IDs) pass through.
- Read + write on the same entity is allowed.
- Unknown verb (no `write_keys` implementation) does not block the batch.
- Structured error includes op indexes and the conflicting key.

## References

- ADR-020: Request DSL — batch semantics, partial success, no cross-op transaction
- ADR-028: Request Parser Crate — `ParsedOp`, `ParsedRequest`
- ADR-025: Pack Standard — `PackRuntime` trait extension point
- `crates/khive-mcp/src/server.rs:232`: `run_parsed` (current `join_all` dispatch)
- `crates/khive-db/src/pool.rs:49`: writer `Mutex` serialization model

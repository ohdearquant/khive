# SubstrateCoordinator Design

**ADRs**: ADR-003 (system architecture), ADR-029 (coordinator layer)
**Last reviewed**: 2026-06-06

## Overview

The coordinator owns all cross-backend operations inside `kkernel`. Pack crates do not depend on
it — they receive a single-backend `KhiveRuntime`. The coordinator routes across backends above
the pack layer.

## Architecture

```text
kkernel::coordinator
  mod.rs  — SubstrateCoordinator + BackendRegistry + LocatorCache
```

Sub-modules (`edges`, `traversal`, `curation`, `health`) are reserved per ADR-029 for D5/D6
work that is not yet implemented.

## Implementation Phases

### D1 — BackendRegistry (shipped)

`BackendRegistry` stores backends in a `BTreeMap<String, BackendEntry>` for deterministic
iteration order. The first registered backend is the primary.

### D2 — LocatorCache (shipped)

`LocatorCache` maps substrate UUIDs to the backend that owns them. Entries expire after 5 minutes
(configurable via `with_locator_ttl`). Eviction is lazy on read. `purge_expired` is available for
maintenance tasks.

`locate(id, namespace)` checks the cache first; on a miss it concurrently probes all backends and
populates the cache on first hit.

### D3 — Fan-out search (shipped)

`fan_out_search(query, namespace, limit)` broadcasts `hybrid_search` to all registered backends in
parallel. Results are merged with Reciprocal Rank Fusion (unweighted, k=60). Per-backend errors are
captured in `BackendSearchResult::error` — a single failing backend does NOT abort the fan-out.

When `is_single_backend()` is true the fan-out degenerates to a single backend call.

### D4 — Cross-backend traversal (deferred)

BFS across backend boundaries following `contains`/`extends`/`depends_on` edges. The coordinator
intercepts `traverse()` results, checks each node's backend via `locate()`, and recursively fans
out to the owning backend. Entry point: `cross_backend_traverse(roots, max_depth, relations, ns)`.

### D5 — WAL cascade on hard-delete (deferred)

When a node is hard-deleted, cascade the delete to all incident cross-backend edges using a WAL
journal. On delete, look up WAL entries for the UUID and issue compensating `delete_edge` calls to
each referenced backend. Entry point: `cascade_delete(id, namespace)`.

### D6 — Backend health map (deferred)

Coordinator maintains a health score per backend derived from consecutive error counts and last
successful call timestamp. `fan_out_search` skips unhealthy backends (score below threshold).
Requires a background health-check loop and a `BackendHealthMap`. Entry point: `health_map()`.

## Single-backend behaviour

When only one backend is registered, every D1–D6 mechanism degenerates to its trivial identity:
no fan-out, no cross-backend routing, no health map misses. Multi-backend complexity is opt-in
via `config.toml` (ADR-028).

## Invariants

- `BackendRegistry` is append-only after boot; no backend is removed at runtime.
- The primary backend is always the first registered.
- `LocatorCache` entries are immutable once inserted (backend affinity is stable per entity).
- `fan_out_search` never panics on per-backend errors; errors are captured in the result.

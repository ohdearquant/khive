# ADR-029: SubstrateCoordinator — Cross-Backend Operations

**Status**: accepted\
**Date**: 2026-05-23\
**Authors**: Ocean, lambda:khive

## Context

ADR-028 introduces pack-scoped backends: each pack declares `backend = "main"` (or
`"lore"`, `"archive"`, ...) in `khive.toml`. ADR-031 introduces multi-engine embedding
with a shared engine registry. Once a graph can span multiple physical SQLite files,
three problems neither ADR-028 nor ADR-031 addresses on its own:

**P1 — Substrate-kind reads must federate.** When a caller invokes `search(kind=note,
query="X")`, the ADR-013 contract is "search all notes." If `memory` is on `main.db` and
`lore` is on `lore.db`, the search must touch both backends and merge results.

**P2 — Edges may cross backends.** A `kg` entity on `main.db` should be able to
`annotates`-link to a `lore` atom on `lore.db`. Hard-disallowing this defeats the
unified-graph model.

**P3 — Some operations live above any single runtime.** `link(source_uuid, target_uuid,
relation)` takes two UUIDs the caller does not know how to route. `traverse(roots,
depth=3)` may follow edges across boundaries. `update_entity(uuid, patch)` must locate
the entity's backend before patching. None of these fit cleanly inside a per-pack
`KhiveRuntime`.

The architectural shape needed is the **coordinator pattern** — a layer above the
pack→runtime mapping that owns cross-backend dispatch, the node-location cache, and
cross-backend metrics. RuVector's `ShardCoordinator` is the closest reference
(`Arc<DashMap<ShardId, Arc<GraphShard>>>` with `target_shards` fan-out); oxigraph's
`Storage { kind: StorageKind }` enum is the reference for backend-kind future-proofing
(adopted in ADR-028).

This ADR specifies four tightly-coupled decisions that together form the cross-backend
operations layer:

| Concern                              | Decision                                                                                                                            |
| ------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------- |
| Cross-backend edge representation    | D1: `target_backend` column on `graph_edges`                                                                                        |
| Node-to-backend resolution           | D2: in-memory lazy locator cache                                                                                                    |
| Cross-backend `link()` mechanics     | D3: coordinator-driven, with edge stored on source's backend                                                                        |
| Substrate-kind search fan-out        | D4: unweighted RRF across backends                                                                                                  |
| Cross-backend traversal and curation | D5: DEFERRED in shipped code; target design retained below for transparent BFS, cross-backend merge errors, and hard-delete cascade |
| Partition tolerance                  | D6: DEFERRED in shipped code; target design retained below for degraded reads, hard-fail writes, and backend health state           |

What this ADR does **not** introduce (out of scope):

- Distributed / network query layer (federation across processes). RuVector's `Federation`
  / `ClusterRegistry` is rejected; khive is in-process.
- Transparent re-partitioning. RuVector's `EdgeCutMinimizer` (METIS) is rejected;
  backends are user-intentional, not auto-derived.
- Cross-backend atomic transactions. SQLite WAL is per-backend; cross-backend writes are
  non-atomic. The deferred D5 target design specifies a compensation WAL (`_cross_backend_wal`) intended to make
  hard-delete cascade recoverable but does NOT provide full cross-backend atomicity.

## Decision

### Coordinator lives inside `kkernel`

Per ADR-003, the coordinator is dispatch-layer code, not a public library. Placing it
inside `kkernel` keeps the boundary tight. Module path: `kkernel::coordinator`. Pack
crates do not depend on it.

```text
kkernel
├── coordinator/      ← this ADR
│   ├── edges.rs      (D1 — target_backend column, link mechanics)
│   ├── locator.rs    (D2 — DashMap<Uuid, BackendName>)
│   ├── search.rs     (D4 — substrate-kind fan-out + RRF)
│   ├── traversal.rs  (D5 — deferred target: cross-backend BFS)
│   ├── curation.rs   (D5 — deferred target: update / merge / delete cascade)
│   └── health.rs     (D6 — deferred target: partition tolerance)
└── (other kkernel modules)
```

A separate `khive-coordinator` crate is rejected: a separate crate adds compile units and
a public surface for what is fundamentally internal kernel plumbing. Packs do not need
it; no external consumer needs it.

### D1 — Edges store `target_backend` on the source's backend

The `graph_edges` table gains a new nullable column:

```sql
ALTER TABLE graph_edges ADD COLUMN target_backend TEXT NULL;
CREATE INDEX idx_graph_edges_target_backend
    ON graph_edges(target_backend)
    WHERE target_backend IS NOT NULL;
```

Semantics:

- `target_backend IS NULL` → target lives on the same backend as the source (the default
  for single-backend deployments; the value for every existing row at migration time).
- `target_backend = "<name>"` → target lives on the named backend (must match a declared
  `[[backends.name]]` per ADR-028).
- The edge row **always lives on the source's backend**. The target's backend is
  referenced by name; the target's backend does not receive a mirror row.

The existing unique constraint `UNIQUE(namespace, source_id, target_id, relation)` is
preserved — uniqueness is per (source-namespace, source, target, relation), independent
of where the target lives.

Migration is purely additive (nullable column + WHERE-indexed partial index); applies via
ADR-015's `VersionedMigration` mechanism. Single-backend deployments inherit
`target_backend IS NULL` on every row and observe no behavioral change.

Storage layer types gain a single field:

```rust
pub struct Edge {
    pub id: LinkId,
    pub source_id: Uuid,
    pub target_id: Uuid,
    pub relation: EdgeRelation,
    pub weight: f64,
    pub created_at: DateTime<Utc>,
    pub metadata: Option<serde_json::Value>,
    pub target_backend: Option<String>,    // NEW
}
```

### D2 — Node locator is an in-memory lazy cache

The coordinator owns:

```text
Arc<DashMap<Uuid, BackendName>>
```

Semantics:

- **Populated on write**: every `create` of an entity or note records its UUID →
  owning-backend mapping in the cache.
- **Lazy on read**: a `locate(uuid)` call hits the cache first; on miss, the coordinator
  issues parallel reads to all backends and caches the first hit (or returns `None` if no
  backend contains the UUID).
- **Not persisted**: in-memory only. Across process restarts the cache is empty and warms
  lazily as queries arrive.

Memory budget: 16 bytes (UUID) + ~16 bytes (backend name) ≈ 32 bytes per entry. 1M cached
entries ≈ 32 MB. Bounded by working set. Currently unbounded; a bounded LRU with operator-
configurable cap is a follow-up if a deployment hits memory pressure.

Cache invalidation:

- **Hard-delete of node X**: invalidate `locator[X]` immediately; coordinator then walks
  backends to remove incoming cross-backend edges (D5 cascade).
- **Soft-delete**: no locator change — the node remains locatable; query layers filter by
  `deleted_at IS NULL`.
- **Hard-delete batch**: coordinator invalidates locator entries in bulk before issuing
  per-backend deletes.
- **Process restart**: cache empties; lazy repopulation as queries arrive.

### D3 — `link()` is coordinator-driven

The caller of `link(source_uuid, target_uuid, relation, weight)` passes UUIDs only — no
backend hints. The coordinator:

1. Resolves source's backend via `locate(source_uuid)`. On miss, parallel-fetch fallback.
   Failure → `UnknownNode(source_uuid)`.
2. Resolves target's backend via `locate(target_uuid)`. Same fallback. Failure →
   `UnknownNode(target_uuid)`.
3. Validates the `(source_kind, relation, target_kind)` tuple against ADR-002's base
   ontology and ADR-017's pack-extensible rules. Violation → `EdgeRuleViolation`.
4. Writes the edge on **source's backend**:
   - Same backend: `target_backend = NULL`.
   - Different backends: `target_backend = "<target_backend_name>"`.
5. Increments the cross-backend edge counter if cross-backend.

The unique constraint and pack-extensible endpoint rules remain authoritative — D3
changes the backend resolution path, not the validation contract.

Reserved error variant `CrossBackendDisallowed(relation)` is in the coordinator's error
enum to allow future tightening if any relation must remain backend-local. No relation
currently triggers this; the variant is a forward-compatibility hook.

### D4 — Substrate-kind search fuses with unweighted RRF across backends

The coordinator maintains a map:

```text
SubstrateKind → Vec<Arc<KhiveRuntime>>
```

Each substrate kind lists the runtimes (and therefore the backends) hosting that kind.
For `search(kind=note, query=...)`:

1. Look up the runtimes hosting `note` and fan out the search to each.
2. Each runtime executes a single-backend `search()` (its existing path).
3. Per-backend results are ranked lists of `(uuid, score)` pairs.
4. Fuse via `khive_fusion::FusionStrategy::Rrf { k: DEFAULT_RRF_K }` — **unweighted**.
5. Fused result is truncated to the requested `top_k` and returned.

**Why unweighted RRF**:

Backends are isolation boundaries, not relevance signals. A user's `main` and `lore`
backends each store notes; neither is intrinsically "more authoritative" at the backend
level. The relevance signals that DO matter — model quality (per ADR-031: BGE vs mE5 vs
Qwen3) and lexical match strength (FTS5 BM25) — operate **inside** a single backend,
where comparisons are calibrated.

A per-backend `weight` config field would invite operators to set weights guessing what
they mean, with no measurable effect on retrieval quality. Removing the knob removes the
footgun. Engine weights inside each backend (ADR-031 calibration) are the right place to
tune.

**Why RRF, not raw-score fusion**:

Per-backend scores are not directly comparable. Backend A's top hit at score 0.84 from
a BGE pipeline is not the same scale as Backend B's top hit at score 0.79 from an mE5
pipeline. RRF uses **rank position only**, sidestepping the scale problem.

`DEFAULT_RRF_K` matches `khive-fusion`'s engine-level fusion constant (k=60).

Plan shape:

```rust
pub struct SubstrateSearchPlan {
    /// Sealed namespace scope. Constructed only from a verified NamespaceToken
    /// at plan-build time. Not a raw string — callers cannot mutate or supply.
    pub scope: NamespaceScope,

    /// High-level retrieval query. Embedding generation happens inside the
    /// pack/backend runtime, not at the coordinator. The coordinator does NOT
    /// own engine-level RRF or embedding model selection.
    pub query: RetrievalQuery,

    /// Routes already constrained to the namespace scope.
    pub target_routes: Vec<ScopedBackendRoute>,

    pub top_k: usize,
    pub min_score: Option<f32>,

    /// Backend-level fusion strategy (unweighted across backends).
    /// Engine-level (weighted) RRF runs INSIDE the pack/backend runtime per ADR-031.
    pub backend_fusion: BackendFusionStrategy,
}
```

`NamespaceScope` is sealed: it is constructed only by the coordinator from a verified `&NamespaceToken`, never from a string. For privileged cross-namespace operations, the coordinator accepts an `AdminNamespaceToken` (or equivalent) issued only by the auth gate (ADR-018). Wildcards like `None`, `"*"`, or `Option<&str>` are not overloaded for cross-tenant scope.

### Engine-level vs backend-level fusion

Per ADR-031, weighted engine-level RRF (fusing per-embedding-engine candidate lists with engine weights) runs inside the pack/backend runtime. The coordinator only fuses already-fused result lists across backend instances using unweighted backend-level RRF. The coordinator does NOT see individual query vectors, embedding models, or engine weights — those are pack/backend internals.

If a backend-local plan needs N vectors (multiple embedding engines), that is an internal pack/runtime detail, not a coordinator-level decision.

**Substrate vs. granular**: only substrate-kind search federates. Granular kinds (`task`
per ADR-019, `memory` per ADR-021, future pack kinds) are pack-owned per ADR-017 and stay
pack-local — they go to whichever backend the owning pack is assigned to.

| Verb                  | Federation                                              |
| --------------------- | ------------------------------------------------------- |
| `search(kind=note)`   | substrate → federates across all backends hosting notes |
| `search(kind=entity)` | substrate → federates                                   |
| `search(kind=task)`   | granular → goes to gtd's backend only                   |
| `search(kind=memory)` | granular → goes to memory's backend only                |

### D5 — Cross-backend traversal and curation semantics

> Status: DEFERRED in shipped code. This section is retained as target design.
> `kkernel::coordinator` currently reserves the traversal and curation modules and
> does not implement `cross_backend_traverse(...)`, cross-backend merge/update
> routing, or the hard-delete cascade WAL. See
> `crates/kkernel/src/coordinator/mod.rs:25-26` and
> `crates/kkernel/src/coordinator/mod.rs:290-303`.

**`coordinator.traverse(roots, options)`** — BFS where each `neighbors()` call is the
unit operation:

1. Locate the current node's backend via D2 locator or parallel-fetch fallback.
2. Read outgoing edges from that backend, including `target_backend` field.
3. For each neighbor:
   - `target_backend = NULL` (or matches source's backend) → resolve locally.
   - `target_backend = "<name>"` → fetch the neighbor from the named backend.
4. Continue BFS with the resolved neighbors.

Pack handlers do not see the boundary. They call `coordinator.traverse(...)` and receive
a unified `Vec<PathNode>` where each node knows the backend it came from (for
observability; ignored by simple consumers).

`neighbors(uuid, direction=Out)` reads outgoing edges from one backend (source's).
`neighbors(uuid, direction=In)` is **asymmetric** — incoming edges to `uuid` may originate
from any backend, so the coordinator fans out across all backends. This is the known cost
of source-side edge storage; the alternative (mirror cross-backend edges) was rejected
(see Alternatives §C).

**`update_entity(uuid, patch)`** — works across backends:

1. Locate the entity's backend via locator (or parallel-fetch fallback).
2. Route the patch to that backend's `KhiveRuntime`.
3. Single-row write inside the entity's backend's SQLite transaction.

Caller observes no difference from local update except possibly higher first-call latency
on locator miss.

**`merge_entity(into_id, from_id)`** — errors when cross-backend:

- Same backend: standard merge per ADR-014.
- Different backends: `CrossBackendMergeUnsupported { into_backend, from_backend }`.

Merge requires moving entity rows between tables and re-pointing every incident edge. A
cross-backend merge would need a 2PC protocol or coordinator compensation log — both
scope for a future ADR. Operators with the use case manually export `from_id`, delete it,
and re-import on `into_id`'s backend.

**`hard_delete_entity(uuid)`** — coordinator cascades incoming cross-backend edges via
a per-owner-backend compensation WAL (`_cross_backend_wal`). The local hard-delete and the
cascade-replay intent commit together; replay happens after commit and is idempotent.

WAL table (one per backend; migrated via standard `khive-db` migration):

```sql
CREATE TABLE _cross_backend_wal (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    op_id           TEXT    NOT NULL,        -- groups all steps of one user-visible op
    step_id         TEXT    NOT NULL UNIQUE, -- deterministic per (op_id, apply_backend, subject_id)
    op_kind         TEXT    NOT NULL CHECK (op_kind IN ('hard_delete_cascade')),
    op_version      INTEGER NOT NULL DEFAULT 1,
    owner_backend   TEXT    NOT NULL,        -- backend physically storing this WAL row
    apply_backend   TEXT    NOT NULL,        -- backend that must apply the compensation
    subject_id      TEXT    NOT NULL,        -- deleted entity/note UUID
    subject_kind    TEXT    NOT NULL CHECK (subject_kind IN ('entity', 'note')),
    subject_backend TEXT    NOT NULL,        -- backend the subject was owned by at delete time
    namespace       TEXT    NULL,
    payload_json    TEXT    NOT NULL DEFAULT '{}',
    status          TEXT    NOT NULL CHECK (status IN ('pending', 'applied', 'dead_letter')),
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    next_attempt_at INTEGER NOT NULL DEFAULT 0,
    last_attempt_at INTEGER NULL,
    completed_at    INTEGER NULL,
    attempt_count   INTEGER NOT NULL DEFAULT 0,
    applied_count   INTEGER NULL,
    last_error_code TEXT    NULL,
    last_error      TEXT    NULL,
    lease_owner     TEXT    NULL,            -- worker process id (for concurrent replay safety)
    lease_expires_at INTEGER NULL,
    UNIQUE(op_id, op_kind, apply_backend, subject_id)
);
```

Indexes: `(status, next_attempt_at, apply_backend, id) WHERE status='pending'`,
`(subject_id, subject_backend, status)`, `(op_id)`.

Note: the WAL column is named `apply_backend`, not `target_backend`, to avoid collision
with the `target_backend` column on `graph_edges` (D1) — they have different meanings.

**Write protocol** (owner backend transaction, BEGIN IMMEDIATE):

1. Acquire per-subject mutation lock (prevents racing `link(x, uuid)` from creating an
   edge after the cascade plan is computed but before commit).
2. Build CascadePlan: enumerate non-owner backends that may have incoming edges to `uuid`.
   A reachable backend is checked directly; an unreachable backend is conservatively
   included as pending.
3. In one transaction on the owner backend:
   - Insert one `pending` WAL row per `apply_backend` in the plan (using deterministic
     `step_id` + `INSERT OR IGNORE` for retry safety).
   - Delete local outgoing edges (`source_id = uuid`) and local incoming edges
     (`target_id = uuid AND (target_backend IS NULL OR target_backend = owner)`).
   - Delete the entity/note row.
4. Commit.
5. Mark `locator[uuid] = Deleted { owner_backend, deleted_at, op_id }`.
6. Attempt immediate replay for each pending row whose `apply_backend` is `Healthy`.

**Replay** is idempotent and safe to repeat:

```sql
DELETE FROM graph_edges
WHERE target_id = :subject_id
  AND target_backend = :subject_backend
  AND (:namespace IS NULL OR namespace = :namespace);
```

A replay step that deletes zero rows still counts as success (means a prior attempt
already cleaned, or no matching edges ever existed). Replay marks the row `applied` and
records `applied_count`.

**Replay timing**: boot scan + lazy retry on partition recovery + manual replay via CLI.
A periodic sweeper is optional (must respect `next_attempt_at` and backend cooldown).
Backoff schedule: 30s, 1m, 2m, 5m, 15m, 1h (capped, jittered). Pending retryable rows
are **never auto-purged** — they remain until success, manual purge, or transition to
`dead_letter`.

**Dead-letter conditions** (terminal, requires operator action):

- `apply_backend` is configured `read_only = true` (can't delete the dangling edges).
- `apply_backend` is unknown / removed from config.
- `apply_backend` schema is incompatible / missing the `graph_edges` table.

**Crash safety**:

| Crash point                                            | Behavior                                          |
| ------------------------------------------------------ | ------------------------------------------------- |
| Before owner transaction commit                        | No delete, no WAL rows. Safe.                     |
| After owner commit, before any cascade                 | Entity gone; WAL rows pending; boot replay fixes. |
| After cascade succeeds, before WAL update to `applied` | Replay deletes 0 rows, still succeeds. Safe.      |
| During applied-status update                           | Same as above. Safe.                              |

Dangling edges left by `pending` WAL rows are filtered at query time. Operators can
inspect via `kkernel db wal status` and force replay via `kkernel db wal replay`. A
backstop `kkernel db cleanup dangling-edges` exists for edges no WAL row can recover
(pre-WAL data, lost owner backend, manual DB edits).

**Result shape**:

```rust
pub struct DeleteSummary {
    pub deleted: bool,
    pub hard: bool,
    pub wal_op_id: Option<Uuid>,
    pub partial_cascade: bool,
    pub cascade_pending_backends:     Vec<BackendName>,
    pub cascade_dead_letter_backends: Vec<BackendName>,
}
```

### Operation matrix

Normative cross-backend semantics:

| Operation                    | Same-backend           | Cross-backend                                           |
| ---------------------------- | ---------------------- | ------------------------------------------------------- |
| `create(kind=entity)`        | local write            | N/A — pack determines backend                           |
| `create(kind=note)`          | local write            | N/A — pack determines backend                           |
| `link(a, b, rel)`            | local edge             | edge on a's backend with `target_backend = b's backend` |
| `get(uuid)`                  | local read             | local read after locator hit                            |
| `update_entity(uuid, patch)` | local write            | local write after locator hit                           |
| `merge_entity(a, b)`         | local merge            | **error** `CrossBackendMergeUnsupported`                |
| `delete(uuid, soft)`         | local mark             | local mark after locator hit                            |
| `delete(uuid, hard)`         | local delete + cascade | local delete + coordinator cascades across backends     |
| `neighbors(uuid, Out)`       | local query            | local edges + cross-backend target resolution           |
| `neighbors(uuid, In)`        | local query            | **fan-out across all backends**                         |
| `traverse(roots, depth)`     | local BFS              | BFS following local + cross-backend edges               |
| `search(kind=note)`          | local search           | fan-out + unweighted RRF                                |
| `search(kind=task)`          | local                  | N/A — task is pack-owned, single backend                |

### D6 — Partition tolerance

> Status: DEFERRED in shipped code. This section is retained as target design.
> `kkernel::coordinator` currently has no `BackendHealthMap`, cooldown loop, or
> `health_map()` entry point; fan-out search reports per-backend errors but does
> not maintain partition-health state. See
> `crates/kkernel/src/coordinator/mod.rs:305-310`.

The coordinator maintains a per-backend health map:

```text
Arc<DashMap<BackendName, BackendHealth>>
where BackendHealth = Healthy | Unreachable { marked_at: Instant }
```

**Detection — passive, not active.** A backend becomes `Unreachable` only when an
operation against it fails with an I/O error. There is no heartbeat in v1.

**Cooldown**: when a backend is marked `Unreachable`, the coordinator skips it for a
configurable cooldown (`backend_unreachable_cooldown_ms`, default 30s). After cooldown,
the next operation against that backend retries. Success returns it to `Healthy`; failure
restarts the cooldown.

**Operation behavior under partition**:

| Op category                         | If a relevant backend is unreachable     | Result shape                         |
| ----------------------------------- | ---------------------------------------- | ------------------------------------ |
| Substrate-kind read (search/list)   | Skip backend, proceed                    | `partial: true` + `missing_backends` |
| Granular-kind read                  | Hard error                               | `PackUnavailable`                    |
| Cross-backend traversal             | Walk reachable, mark terminated branches | `partial: true`                      |
| Cross-backend hard-delete cascade   | Cascade reachable; skip unreachable      | `partial_cascade: true`              |
| Any write to an unreachable backend | Hard error immediately                   | `BackendUnreachable`                 |

**The principle: reads degrade, writes don't.** The `partial` flag is the only way a
caller observes degradation — silent skipping is forbidden.

## Single-backend default behavior

For a deployment with one `[[backends.main]]` entry hosting all packs:

- D1: every edge has `target_backend = NULL`; never observed.
- D2: locator has one backend; never misses (all UUIDs map to `main`).
- D3: `link()` always sets `target_backend = NULL`; identical to pre-coordinator behavior.
- D4: substrate-kind fan-out is one-target; RRF over one ranked list is identity.
- D5: traversal walks one backend; merge always same-backend; cascade iterates zero
  cross-backend backends.
- D6: no other backends to be unreachable; partial flags never raised.

Coordinator is **zero behavioral change** on single-backend deployments. Multi-backend
complexity is opt-in via TOML.

## Rationale

### Why not RuVector's pure-locator approach

RuVector edges store only `(from_id, to_id)`; cross-shard locality is resolved at runtime
by the coordinator's node-to-shard map. The decision hinges on khive's operational
shape, which differs from RuVector's:

| Property                                    | Pure-locator            | `target_backend` column            |
| ------------------------------------------- | ----------------------- | ---------------------------------- |
| Cold-start cross-backend traversal          | needs locator warmup    | works immediately                  |
| Isolated-backend introspection              | impossible              | SQL query                          |
| Observability metric (main→lore edge count) | requires full edge scan | SQL aggregate                      |
| Storage cost                                | -1 column               | +1 nullable column                 |
| Edge-stale risk if target moves             | none                    | bounded — no auto-migration policy |

RuVector's shards are designed for graph operations (the locator is hot, fully populated,
expected). khive's backends are **intentional isolation boundaries** — cross-backend edges
are exceptional, the locator is sparse and cold by default, and operators care about
which backend hosts which nodes for backup/restore reasons. Persisting locality on the
edge row gives properties the locator alone cannot.

### Why not oxigraph's named-graph approach

oxigraph stores `graph_name` on every quad. That places logical isolation inside one
physical store. khive uses **both** layers:

- `namespace` column on every row — equivalent of oxigraph's named graph; logical
  isolation within a backend.
- Multiple `[[backends]]` entries — physical isolation across SQLite files.

The intentional-isolation model needs the physical layer.

### Why coordinator inside `kkernel`, not a separate crate

A separate `khive-coordinator` crate adds compile units and a public surface for
fundamentally internal kernel plumbing. Packs do not need it; no external consumer needs
it. RuVector keeps its `Coordinator` inside `ruvector-graph::distributed` for the same
reason.

### Why passive partition detection

Active health checks (heartbeats) add complexity (timing, false positives, separate
thread) for a property that is otherwise free (failing operations are the most reliable
unreachability signal). Passive detection has zero cost when all backends are healthy.
"Unreachable" is a hint for subsequent operations, not a guarantee.

### Why reads degrade and writes don't

Multi-backend deployments benefit from isolation: an `archive` backend being offline
should not break searches over the `main` backend's notes. Returning partial results with
an explicit `partial: true` flag is correct.

Writes are different: a successful `link(main_entity, lore_atom)` with lore unreachable
would silently lose the cross-backend edge. Hard-failing keeps writes consistent; the
operator chooses when to retry.

## Alternatives Considered

### A. No coordinator — pack handlers do their own cross-backend work

Pack handlers receive `HashMap<BackendName, Arc<KhiveRuntime>>` and orchestrate fan-out
themselves. Pros: no new kernel component. Cons: every pack reimplements substrate-kind
dispatch, locator, cross-backend cascade — a de-facto coordinator copy-pasted across
packs. Routing decisions become pack code, making operational tuning impossible without
recompiling.

Rejected. Centralizing in `kkernel` keeps pack code single-backend — packs are about
semantics, not topology.

### B. Per-pack coordinator inside each pack crate

Each pack ships its own coordinator. Pros: pack autonomy. Cons: substrate-kind dispatch
needs a coordinator that sees ALL backends — that cannot live inside one pack.

Rejected.

### C. Two-sided storage (mirror cross-backend edges on both backends)

Store the edge on both source's and target's backends so incoming queries are fully
local. Pros: `neighbors(uuid, In)` becomes O(1). Cons: doubles cross-backend edge
storage; introduces non-atomic two-backend write on `link()`; cascade-on-delete is more
complex.

Rejected. The fan-out at neighbors-In time is acceptable cost.

### D. Out-of-process coordinator (microservice)

Coordinator is its own process; packs and `kkernel mcp` talk to it over IPC. Pros: clean
isolation. Cons: dramatic complexity; per-call IPC overhead on every verb; defeats the
in-process MCP daemon model.

Rejected.

### E. Defer cross-backend operations entirely

Disallow cross-backend operations. `link(a_on_main, b_on_lore)` returns
`CrossBackendLinkUnsupported`. Pros: simplest. Cons: defeats the unified-graph model;
substrate-kind search becomes wrong (`kind=note` returns only one backend's notes).

Rejected.

### F. Cross-backend merge via 2PC

Implement a real two-phase commit for `merge_entity` across backends. Pros: full
consistency. Cons: substantial complexity (write-ahead log, compensation, partial-failure
recovery) for a rarely needed operation.

Rejected for v1. Operators with the use case use export+delete+import. Future ADR can add
2PC when concrete need emerges.

### G. Per-backend weighted RRF in substrate search

Configure a `weight` per backend; substrate search fuses with `FusionStrategy::Weighted`.
Pros: knob for tuning. Cons: backends are isolation boundaries, not relevance signals;
the weight has no calibration target.

Rejected.

### H. Active health checks (heartbeat)

Background task pings each backend. Pros: faster recovery detection. Cons: complexity not
justified; passive detection works on operations users care about.

Rejected.

### I. Coordinator-level transaction log for cascade idempotency

Persist cross-backend cascade operations so they can be replayed on failure. Pros: clean
cascade semantics, recoverable partition behavior. Cons: write-amplification on every
hard_delete (~10s of bytes per cascade backend); complexity vs simple "dangling + cleanup."

**Accepted.** The per-owner-backend `_cross_backend_wal` (see D5 above) is the v1 design.
Rationale: making hard-delete cascade recoverable is a much better invariant than
"some edges might dangle, run cleanup later." The write amplification is small (one row
per non-owner backend, only on hard-delete), and the WAL pays for itself the first time
a backend reboots mid-cascade. The original concern ("complexity for a rare issue") was
right that the WAL adds machinery, but wrong about how often partitions happen in practice
— any operator running multiple SQLite files across mounts will hit it.

The WAL is **only** for hard-delete cascade compensation, not a general cross-backend
write queue. `link()` and ordinary writes still hard-fail on partition (see D6).

## Consequences

### Positive

- **Unified graph view across backends** — `traverse`, `link`, `search(kind=substrate)`
  work uniformly.
- **Pack code stays single-backend** — coordinator owns the boundary.
- **Zero behavioral change for single-backend deployments** — every D1-D6 mechanism
  degenerates to identity.
- **Cold-start cross-backend traversal works** — `target_backend` column gives immediate
  introspection without locator warmup.
- **Per-backend isolated introspection** — "what does main link to?" is a SQL query
  against `target_backend`.
- **Partition-tolerant reads** — explicit `partial: true` flag; never silent skipping.

### Negative

- **Cross-backend operations are non-atomic** — D5 cascade and the local hard-delete
  commit together on the owner backend, but remote backends apply asynchronously via the
  `_cross_backend_wal`. Dangling edges may exist between commit and replay; they are
  filtered at query time and removed by replay. `partial_cascade: true` surfaces the
  state to the caller; `kkernel db wal status` exposes it to operators.
- **Cross-backend merge unsupported in v1** — `CrossBackendMergeUnsupported`. The WAL
  handles idempotent cleanup, not multi-object rewires. Workaround: manual export/import.
- **WAL adds write amplification on hard-delete** — one INSERT per non-owner backend in
  the cascade plan. Bounded; only on `hard_delete_entity(hard=true)`.
- **Incoming neighbors are O(N backends)** per node. Bounded by backend count and
  visited-set pruning.
- **Locator memory budget grows with working set** — bounded but unmonitored in v1.
- **More test surface** — substrate-kind tests, cross-backend link tests, partition tests.

### Neutral

- **ADR-002's 15 edge relations** (closed taxonomy) — `target_backend` is row metadata,
  not a relation. (Amended by ADR-055: current total is 17 edge relations.)
- **ADR-013 cross-substrate search contract preserved** — D4 fulfills it for
  multi-backend.
- **MCP wire format unchanged** — clients see the same verbs; backend assignment is
  invisible.
- **`khive-storage` largely unchanged** — adds `target_backend` field on `Edge`.

## Migration

ADR-028 already covered single-backend default config. This ADR's `target_backend` column
adds a nullable column to `graph_edges` via ADR-015's `VersionedMigration` mechanism. No
data churn; existing rows inherit `NULL`.

`kkernel` boot constructs the coordinator from the assembled per-pack runtimes. The
construction is zero-cost on single-backend deployments — the coordinator's tables are
empty (one entry per substrate kind, all pointing at `main`).

## Open Questions

1. **Locator eviction policy.** Default unbounded for v1; bounded LRU with operator-
   configurable cap if a deployment exceeds ~10M cached UUIDs.
2. **Stable backend IDs vs. names.** `target_backend` references operator-defined
   strings. Renaming a backend orphans existing values. Mitigation v1: document that
   backend renames require a migration script. Future: a stable `backend_id` UUID
   alongside names.
3. **`CrossBackendDisallowed` concrete trigger.** Reserved error variant has no current
   rule. Either commit to a concrete rule or drop the variant until the design exists.
4. **Cascade idempotency on partial failure.** DEFERRED with D5 target design: the
   `_cross_backend_wal` is the intended idempotent per-backend retry mechanism.
   Pending rows should survive process restarts; replay should delete 0 rows on
   already-clean targets without error once D5 is implemented.
5. **Per-backend cooldown configurability via TOML.** Default 30s for all backends.
   Per-backend override is one field on `BackendConfig` (ADR-028); add if operators ask.
6. **Health introspection admin command.** `kkernel debug backend health` to print the
   health map. Not in v1 scope.

## References

- [ADR-002](ADR-002-edge-ontology.md) — 15-relation closed ontology; unchanged
- [ADR-003](ADR-003-system-architecture.md) — `kkernel` is the coordinator's home
- [ADR-009](ADR-009-backend-architecture.md) — multi-file SQLite federation; this ADR
  realizes cross-backend ops above it
- [ADR-013](ADR-013-note-kind-taxonomy.md) — substrate vs granular kinds; the dispatch
  discriminator for D4
- [ADR-014](ADR-014-curation-operations.md) — single-backend curation baseline that D5
  extends
- [ADR-015](ADR-015-schema-migrations.md) — migration mechanism for `target_backend`
  column
- [ADR-017](ADR-017-pack-standard.md) — pack-extensible edge endpoints consulted by D3
- [ADR-028](ADR-028-pack-scoped-backends.md) — backends declared here are the
  coordinator's targets
- [ADR-031](ADR-031-multi-engine-retrieval.md) — engine-level fusion is per-backend;
  this ADR's D4 is at a different layer (backend-level)
- RuVector `crates/ruvector-graph/src/distributed/coordinator.rs` — shape adapted
- oxigraph `lib/oxigraph/src/storage/mod.rs` — `StorageKind` pattern referenced

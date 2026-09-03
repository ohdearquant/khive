# ADR-009: Backend Architecture

**Status**: accepted\
**Date**: 2026-05-23\
**Authors**: khive maintainers\
**Amended by**: [ADR-111](ADR-111-blob-store.md) adds `BlobStore`; [ADR-121](ADR-121-attachments-first-class.md)
and ADR-160 Phase 4 add `AttachmentStore`. References below to eight traits describe the original
surface; the current public capability enum has ten variants.

## Context

khive persists its knowledge graph in SQLite. The storage layer is split: `khive-storage`
defines capability traits (the original eight in ADR-005, later extended by ADR-111/121);
`khive-db` implements SQLite storage traits,
including FTS5 `TextSearch` and the current sqlite-vec `VectorStore`. Separate retrieval
crates (`khive-retrieval`, `khive-bm25`, `khive-hnsw`, `khive-vamana`, `khive-fusion`)
ship in-process retrieval engines and fusion.

The backend architecture must satisfy:

1. **Embedded deployment.** khive ships as a single Rust binary with zero external service
   dependencies. No separate database process, no network database, no managed service
   required.
2. **Multi-file federation.** Different packs may use different SQLite files (hot KG vs cold
   corpus vs archive). This is multiple SQLite files in one process, not a distributed
   database.
3. **Trait portability.** The storage traits (ADR-005 and amendments) are the contract boundary.
   A future non-SQLite backend must be possible without changing runtime or pack code.
4. **Vector search.** The current `khive-db` VectorStore is sqlite-vec compatibility.
   In-process retrieval engines also ship as separate crates, and pack-specific paths may
   layer Vamana ANN or khive-retrieval fusion above storage. External vector database
   services are not part of the embedded backend story.

## Decision

### SQLite-first

khive's v1 backend is SQLite. The concrete backend crate is `khive-db`.

`khive-db` implements the original eight `khive-storage` capability traits (ADR-005), plus the
later blob and attachment capabilities:

- `SqlAccess` — raw SQL reader/writer/transaction
- `EntityStore` — entity/node CRUD
- `GraphStore` — link/edge CRUD and graph traversal
- `NoteStore` — note substrate CRUD
- `EventStore` — append-only event log
- `VectorStore` — current dense vector storage/search via sqlite-vec compatibility
- `SparseStore` — sparse vector storage
- `TextSearch` — full-text search via FTS5 trigram
- `BlobStore` — content-addressed object storage and transactional filesystem GC
- `AttachmentStore` — role-keyed entity/note blob-reference metadata
- Retrieval engines — BM25, HNSW, Vamana, and fusion live outside `khive-db`

`khive-db` supports both file-backed and in-memory storage. In-memory mode is used for
tests and ephemeral deployments.

### `khive-db` is the canonical name

`khive-db` will not be renamed to `khive-db-sqlite`. It is already published on crates.io
and referenced by downstream crates. Renaming for symmetry with a hypothetical second
backend is not justified.

If a second backend ever ships, it uses an explicit suffix (e.g., `khive-db-postgres`).
The naming asymmetry is accepted to preserve crates.io continuity.

### One crate per backend

Any new storage engine gets its own crate. The rule:

```text
One backend crate per storage engine.
Current: khive-db (SQLite)
Future:  khive-db-postgres, khive-db-rocksdb, etc. (if approved by ADR)
```

Each backend crate implements the capabilities it advertises under the same `khive-storage`
contracts. The runtime and
packs depend on traits, not on any specific backend crate.

### v1 multi-backend: multiple SQLite files

v1 "multi-backend" means multiple SQLite file backends inside one process. Pack-scoped
backend assignment (`khive.toml`) and cross-backend routing are implemented by the
SubstrateCoordinator (ADR-003, ADR-029 (Substrate Coordinator)).

```text
khive.toml (illustrative — backend count and names are deployment decisions,
            not settled by this ADR):
  [[backends]]  main → ~/.khive/khive.db
  [[backends]]  lore → ~/.khive/lore.db
```

**Open decisions**: The number of backends in a default deployment, whether an `archive`
backend is justified, and whether `BackendId` is a `String` or `u16` are not settled by
this ADR. The examples above are illustrative.

Each backend is an independent SQLite file with its own WAL, VACUUM cadence, and cache
configuration. Cross-backend operations (edges, search, traversal) are coordinated above
the backend layer — the backends themselves are unaware of each other.

Single-backend remains the default. Multi-backend is opt-in via TOML configuration.
Observable behavior with one backend is identical to pre-federation khive.

### `target_backend` invariant

The `target_backend` column on edge records records which backend the target entity
resides on. The invariant:

```text
target_backend IS NULL ↔ target entity is on the local (same) backend.
```

This invariant MUST be enforced by a CHECK constraint or trigger. When `target_backend`
is NULL, the edge is local — no cross-backend resolution needed. When non-NULL, the
SubstrateCoordinator resolves the target to the named backend.

### Edge upsert semantics

`upsert_edge` uses `INSERT ... ON CONFLICT DO UPDATE` (not `DO NOTHING`) for a
live natural-key match. The update refreshes `updated_at`, `weight`, metadata,
and `target_backend`; it is replacement, not metadata merge. The persisted row
ID and original `created_at` survive.

Amendment (2026-08-30): a soft-deleted natural-key row is not a live upsert
target. Default upserts refuse that conflict without changing metadata or
`deleted_at`; restoration requires an explicit `resurrect=true` policy at the
runtime boundary. Observed storage variants return `created`, `updated`, or
`resurrected` with the preimage so the runtime can record an honest audit event.
Legacy storage methods retain their return types and adopt the safe default
(`resurrect=false`).

### Cross-backend delete cascade

Cross-backend hard-delete of an entity requires cascading incident edges on other
backends. This is non-atomic — SQLite WAL is per-file, and there is no 2PC across files.

The B-lite cascade design uses a `_cross_backend_wal` table as a compensation log:

```text
1. Write cascade intent to _cross_backend_wal on source backend (within source tx)
2. Execute edge deletions on each target backend
3. Mark _cross_backend_wal entry as completed
4. On crash recovery: replay incomplete _cross_backend_wal entries (idempotent)
```

Partial cascade failure (entity deleted, some edges remain) is visible: dangling edges
are filtered at query time by the coordinator. The `_cross_backend_wal` ensures eventual
consistency on recovery.

### Cross-backend merge

`merge_entity` across backends returns an error in v1 and v2. Cross-backend merge
requires coordinated updates to edges, properties, and potentially vector/text indexes
across multiple SQLite files. This is not supported — both entities must reside on the
same backend. The error message must state this constraint explicitly.

### Vector portability: in-process only

Vector portability is handled through `VectorStore`, not through external vector database
services.

The current `khive-db` backend uses sqlite-vec for brute-force vector search. HNSW and
Vamana now ship as khive crates (`khive-hnsw`, `khive-vamana`), and retrieval composition
ships in `khive-retrieval` and `khive-fusion`; do not describe `khive-hnsw` as deleted.

**Risk**: sqlite-vec cross-backend search via UNION ALL requires empirical validation.
Performance and correctness of `sqlite-vec` virtual tables across ATTACHed databases
has not been verified.

**ATTACH constraints**: SQLite allows at most 10 ATTACHed databases by default (125
with compile-time override). This is a hard product constraint on the number of
simultaneously active backends per connection.

External vector database services (Qdrant, Weaviate, pgvector) are not part of the v1
backend story. They violate the zero-service embedded deployment model.

### Postgres and Neo4j

Postgres and Neo4j are not on the v1 roadmap. They remain possible future backend engines
because the trait boundary permits them, but they are not planned work and receive no
version commitment in this ADR.

If a non-SQLite engine is proposed, it requires:

1. A new ADR justifying the engine against the embedded deployment constraint.
2. Its own backend crate implementing the required storage traits.
3. Backend contract test coverage matching `khive-db`.

### `StorageError::Unsupported` contract

`StorageError::Unsupported` signals that a backend does not support a capability method
or optional operation. Backends should only advertise capability traits they can
meaningfully serve. Optional methods may return `Unsupported`; required methods should
return `Unsupported` only when a uniform trait object is deliberately installed for a
backend with partial support.

Capability discovery (via `VectorStoreCapabilities` or similar) is preferred over
implementing traits only to fail at every call.

### Backend contract tests

Backend contract tests exercise the advertised storage traits against `khive-db` (both
SQLite memory and SQLite file-backed). They validate that the backend correctly
implements the trait contracts.

```rust
async fn run_backend_contract<B: BackendFactory>(factory: B) {
    test_sql_access(factory.sql()).await;
    test_entity_store(factory.entities()).await;
    test_graph_store(factory.graph()).await;
    test_note_store(factory.notes()).await;
    test_event_store(factory.events()).await;
    test_vector_store(factory.vectors()).await;
    test_sparse_store(factory.sparse()).await;
    test_text_search(factory.text()).await;
}
```

When a second backend ships, the same harness becomes the cross-backend conformance suite.

## Rationale

### Why SQLite (not Postgres)?

khive's deployment model is a single Rust binary that works offline, on a laptop, without
a database server. Postgres requires a running process, network configuration, and
operational overhead. SQLite is an embedded library — no server, no network, no
configuration.

For networked deployments, SQLite's single-writer constraint is acceptable because khive's
write pattern is low-throughput agent operations, not high-concurrency OLTP. If write
throughput becomes a bottleneck, the multi-file federation model (multiple SQLite files
with independent WAL) provides horizontal scaling within the embedded model.

### Why not rename khive-db?

`khive-db` is published and consumed. Renaming creates downstream breakage for naming
symmetry with a backend that does not exist. The cost of asymmetric naming
(`khive-db` vs `khive-db-postgres`) is cosmetic; the cost of renaming is real.

### Why in-process vector (not external)?

An external vector database (Qdrant, Weaviate) adds a service dependency, network
latency, deployment complexity, and a failure mode that doesn't exist in the embedded
model. sqlite-vec provides correct (if brute-force) vector search with zero additional
dependencies. In-process HNSW and Vamana ANN now ship as separate khive crates
(`khive-hnsw`, `khive-vamana`); retiring sqlite-vec from `khive-db` is explicit
future work, not current shipped behavior.

### Why one crate per backend?

Mixing SQLite and Postgres implementations in one crate couples their compile-time
dependencies, feature flags, and test suites. Separate crates keep dependencies isolated:
`khive-db` depends on `rusqlite`; a future `khive-db-postgres` would depend on
`tokio-postgres`. Neither pollutes the other's dependency tree.

### Why multi-file (not single-file)?

Different data profiles need different storage characteristics. A hot KG database and a
300K-atom cold corpus have different VACUUM cadences, cache sizes, backup strategies, and
read-write patterns. Namespace isolation handles tenancy; backend isolation handles
storage profiles. A single SQLite file cannot provide per-corpus VACUUM or per-pack
read-only mode.

## Alternatives Considered

| Alternative                                    | Why rejected                                                                             |
| ---------------------------------------------- | ---------------------------------------------------------------------------------------- |
| Postgres as v1 backend                         | Violates zero-service embedded deployment model.                                         |
| Neo4j as v1 backend                            | No graph-native backend code exists. Adding one requires a new ADR.                      |
| Rename `khive-db` to `khive-db-sqlite`         | Published crate. Breakage for cosmetic symmetry.                                         |
| External vector DB (Qdrant, Weaviate)          | Adds service dependency. Violates embedded model.                                        |
| Single SQLite file for all packs               | Can't isolate VACUUM, can't read-only one slice, can't backup independently.             |
| Multi-backend crate (all engines in one crate) | Couples compile dependencies. SQLite + Postgres in one crate pollutes both.              |
| Backend contract tests deferred                | Immediate regression value. Conformance suite is the same harness with a second backend. |

## Consequences

### Positive

- Zero-service deployment. Single binary, embedded SQLite, no external dependencies.
- Trait boundary preserves future engine portability without committing to building one.
- Multi-file federation provides per-pack storage profiles within the embedded model.
- Backend contract tests catch storage regressions immediately.
- `khive-db` name is stable on crates.io.

### Negative

- SQLite single-writer limits concurrent write throughput.
  Mitigated: agent write patterns are low-throughput; multi-file provides per-pack parallelism.
- No Postgres or Neo4j means khive cannot leverage their query optimizers or scale properties.
  Mitigated: not a v1 requirement. Trait boundary preserves the option.
- sqlite-vec brute-force is slow for >10K vectors.
  Mitigated: `khive-hnsw` and `khive-vamana` ship in-process ANN; pack-specific retrieval
  paths layer them above the `khive-db` VectorStore. Retiring sqlite-vec from `khive-db`
  is a separate future implementation step.
- Cross-backend hard-delete is non-atomic (no 2PC across SQLite files).
  Mitigated: `_cross_backend_wal` compensation log with idempotent replay.
- Cross-backend `merge_entity` is unsupported (v1 and v2).
  Mitigated: error message states the constraint; both entities must be on the same backend.

### Neutral

- `khive-storage` trait surface unchanged by this ADR.
- SQL schema and migrations remain SQLite-specific in `khive-db`.
- MCP wire protocol unchanged — backend architecture is invisible to clients.

## Implementation

- `crates/khive-db/`: SQLite backend implementing the current storage traits.
- `crates/khive-db/src/stores/`: one module per trait implementation (entity, graph,
  note, event, vector, sparse, text, sql).
- `crates/khive-db/src/migrations.rs`: SQLite schema migrations (applied by `kkernel db migrate`, ADR-003).
- `crates/khive-db/src/backend.rs`: `StorageBackend` — the concrete SQLite connection
  wrapper providing `Arc<dyn Trait>` accessors.
- Backend contract tests: trait-specific suites exercising the advertised capabilities.

## Amendment — Guarded replacement for full-row updates (#1753)

This amendment qualifies the following accepted text:

> “`khive-storage` trait surface unchanged by this ADR.”

and:

> “`upsert_edge` uses `INSERT ... ON CONFLICT DO UPDATE` (not `DO NOTHING`).”

### Decision

The storage contract adds guarded full-row replacement for entities and edges, alongside the
existing note replacement primitive:

- `EntityStore::replace_entity_if_unchanged` accepts a complete replacement entity and the
  revision read during preparation. Its write is an update-only compare-and-swap: the entity id,
  expected revision, and expected deletion state must still match, and the replacement revision
  must strictly advance the expected revision.
- `GraphStore::replace_edge_if_unchanged` provides the corresponding contract for a complete edge
  replacement. It compares the edge id, expected revision, and expected deletion state before
  replacing the row; it does not insert a missing edge or silently apply a stale replacement.
- Exactly one affected row is a successful guarded replacement. A missing row, changed revision or
  deletion state, or non-advancing replacement is a distinct optimistic-concurrency conflict,
  surfaced at the storage boundary as `StorageError::Conflict`. It is not `NotFound`, an input
  error, or an undifferentiated backend-driver failure. Callers must not blindly retry the stale
  full row; they must read current state and explicitly reapply or abandon their intended change.

The existing `NoteStore::replace_note_if_unchanged` remains available as the note precedent. Its
current public result is `false` for a refused compare-and-swap, and its strict revision and
deletion-marker predicate remains the reference semantics (`crates/khive-storage/src/note.rs:309-321`;
`crates/khive-db/src/stores/note.rs:114-175,891-905`). This amendment does not remove or repurpose
that method. Backends that do not implement either new method may return the existing
`StorageError::Unsupported` default; they must not fall back to an unguarded upsert.

`StorageError::Conflict` already exists as a public storage error (`crates/khive-storage/src/error.rs:59-64`),
so this amendment does not require a new error variant. If implementation chooses to introduce a
new public error variant instead, adding that variant is a source-compatibility event: downstream
code with exhaustive matches must handle it. The new trait methods likewise require downstream
storage implementations to add an implementation or use an explicitly documented default.

### Existing operations and semantics that do not change

The ordinary `upsert_entity` and `upsert_edge` operations remain insert-or-update operations and
remain usable when the caller intentionally wants their existing behavior. The entity statement
continues to write the full entity row (`crates/khive-db/src/stores/entity.rs:57-82`) and the edge
statement continues to use its existing id and natural-key conflict arms
(`crates/khive-db/src/stores/graph.rs:90-124`); their SQLite conflict behavior is not silently
changed into compare-and-swap. Batch upserts, reads, deletes, cross-backend rules, the SQL schema,
and the MCP wire surface are unchanged. Guarded replacement is an additional opt-in storage seam.

### Scope

This change covers these eight production-graph sites:

1. Runtime entity curation (`crates/khive-runtime/src/curation.rs:853-928`).
2. Runtime non-symmetric edge curation (`crates/khive-runtime/src/operations.rs:5662-5816`).
3. Runtime symmetric edge curation (`crates/khive-runtime/src/operations.rs:5559-5639,5747-5771`),
   which replaces both directed rows in place.
4. Atomic entity plans (`crates/khive-runtime/src/atomic_prepare.rs:804-892`).
5. Atomic non-symmetric edge plans (`crates/khive-runtime/src/atomic_prepare.rs:863-1050`).
6. Atomic symmetric edge plans (`crates/khive-runtime/src/atomic_prepare.rs:1000-1040`).
7. `comm.heartbeat` channel-health persistence (`crates/khive-pack-comm/src/handlers.rs:2321-2411`;
   dispatch at `crates/khive-pack-comm/src/pack.rs:291-292`).
8. Scheduled-event finalization (`crates/khive-mcp/src/pending_events.rs:1113-1128,2149-2213`),
   covering every finalization branch rather than the normal dispatch path alone.

Sites 3 and 6 are the symmetric edge paths. They are in scope because they replace rows through the
same unguarded full-row statement (`crates/khive-db/src/stores/graph.rs:303-306`, whose `WHERE`
clause pins only namespace and id) as the non-symmetric paths. Excluding them would leave the
contract stated here true of one edge path and false of the other, which is not a contract.

Site 8 covers all six finalization branches, not only the normal one. The pre-action, error, missed
and non-action branches finalize on claim identity alone; the guard stated here binds the serialized
properties snapshot as well, because a claim token is an ownership fence and not a content fence, so
a property writer landing between claim and finalization is otherwise overwritten while the
finalizer reports success.

The 19 code-map sites in `khive-pack-code` are not covered by this amendment. They are tracked
separately in public issue #2231.

The source confirms why these sites need the additional seam: entity curation currently reads and
then calls the unguarded entity upsert (`crates/khive-runtime/src/curation.rs:852-856,918-928`),
and the non-symmetric edge path currently calls the unguarded edge upsert
(`crates/khive-runtime/src/operations.rs:5811-5816`). The guarded methods close that
read-to-replacement race without changing the ordinary upsert contract.

### Rejected alternatives

- Relying on the write queue alone is insufficient. The entity store routes a write through the
  pool-wide writer task (`crates/khive-db/src/stores/entity.rs:182-205`), while `get_entity` uses a
  separate reader (`crates/khive-db/src/stores/entity.rs:726-742`); that does not bind the earlier
  read to the later replacement. The guarded predicate is the required safety boundary.
- Blindly retrying a stale row is rejected because it can overwrite the intervening change again.
- Replacing a whole-document race with an unconditional `SET properties = ?` is rejected; it
  still permits a stale document to overwrite concurrent changes.

No open design question remains within this storage amendment: the conflict is a typed storage
outcome, ordinary upserts retain live-row replacement semantics while tombstone restoration is
explicit, and the eight-site boundary above is explicit.

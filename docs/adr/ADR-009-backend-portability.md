# ADR-009: Backend Portability — SQLite, Postgres, Neo4j

**Status**: accepted\
**Date**: 2026-05-15\
**Authors**: Ocean, lambda:khive

## Context

khive needs to run on multiple storage backends:

- **SQLite** (default) — single-file, embeddable, perfect for local research workflows.
- **Postgres** — multi-user, networked, scales to large graphs. pgvector for embeddings, tsvector
  for FTS.
- **Neo4j** — existing graph databases, federation, visualization tooling.

Different deployments will favor different backends:

- A solo researcher's laptop → SQLite (no service to run).
- A research team server → Postgres (concurrent access, durability, backup tooling).
- An organization with existing Neo4j → khive as a complementary tool that interops with their
  graph.

The decision: how do we structure crates so a new backend is a contained addition, not a refactor?

## Decision

**One crate per backend. All implement the same `khive-storage` capability traits.**

### Crate naming

| Crate               | Backend                        | Status                                |
| ------------------- | ------------------------------ | ------------------------------------- |
| `khive-db`          | SQLite + sqlite-vec + FTS5     | v0.1 (current; SQLite implementation) |
| `khive-db-postgres` | Postgres + pgvector + tsvector | planned (v0.3+)                       |
| `khive-db-neo4j`    | Neo4j (Bolt protocol)          | planned (v0.4+)                       |

`khive-db` is the SQLite-backed implementation today. Future backends use a `khive-db-<backend>`
suffix to make the choice explicit at the dependency level. We may also rename the SQLite crate to
`khive-db-sqlite` for symmetry; deferred until a second backend lands so we move the bytes once.

### What each backend implements

All backends implement the same six traits from `khive-storage`:

- `SqlAccess` — base SQL capability (Neo4j backend: returns Unsupported for raw SQL).
- `GraphStore` — edge CRUD + traversal.
- `NoteStore` — temporal record CRUD.
- `EventStore` — append-only operation log.
- `VectorStore` — embedding search.
- `TextSearch` — full-text search.

Consumers depend on `Arc<dyn TraitName>` — they don't know or care which backend is wired up.

### What's different per backend

| Concern          | SQLite                          | Postgres                                     | Neo4j                                                               |
| ---------------- | ------------------------------- | -------------------------------------------- | ------------------------------------------------------------------- |
| Connection model | rusqlite + connection pool      | sqlx + connection pool                       | bolt driver + session pool                                          |
| Vector storage   | sqlite-vec `vec0` virtual table | pgvector `vector(N)` column + `<->` operator | not native — synthesized via similarity functions or external index |
| Full-text search | FTS5 with trigram tokenizer     | `tsvector` + `tsquery`                       | full-text procedures (CALL db.index.fulltext.queryNodes)            |
| Graph traversal  | Recursive CTE                   | Recursive CTE (similar to SQLite)            | Native Cypher MATCH                                                 |
| Concurrency      | WAL mode, 1 writer + N readers  | Postgres MVCC, many writers                  | Bolt sessions                                                       |
| Migration system | Per-service ServiceSchemaPlan   | Same trait, different DDL                    | Cypher CREATE/DROP scripts                                          |

## Rationale

### Why one crate per backend (not feature flags)?

Feature flags on a single `khive-db` crate were considered but rejected:

1. **Compile time**: Each feature adds rusqlite OR sqlx OR neo4j-bolt to the dep graph. Users opting
   into multi-backend get all the deps regardless of which they actually use.
2. **Code complexity**: `#[cfg(feature = "sqlite")]` annotations would litter every file.
   Backend-specific code (vector store, FTS) doesn't share much; the conditional compilation becomes
   a maze.
3. **Independent versioning**: A SQLite-only user shouldn't care about a Postgres-only bug fix.
   Separate crates means independent releases.
4. **Discoverability**: `cargo add khive-db-postgres` is clearer than
   `cargo add khive-db --features postgres`.

### Why all backends implement the same traits (not backend-specific APIs)?

The trait abstraction is the whole point of `khive-storage`. If `khive-db-postgres` exposed
Postgres-specific methods, consumers would couple to Postgres. We'd lose backend swappability.

Backend-specific optimizations (e.g., Postgres's `INSERT ON CONFLICT`) can be used _inside_ the impl
as long as the public API stays trait-conformant.

### Why Postgres specifically (not MySQL/MariaDB)?

- **pgvector**: production-quality vector extension, native HNSW index, integrates with Postgres
  MVCC.
- **tsvector/tsquery**: rich FTS with stemming, ranking, multiple languages.
- **JSONB**: native typed JSON columns — useful for entity `properties` field.
- **Recursive CTE**: graph traversal via standard SQL (same query pattern as SQLite).

MySQL/MariaDB lack pgvector-equivalent maturity; their FTS is weaker. If demand emerges, a
`khive-db-mysql` crate can be added without changing anything else.

### Why Neo4j (not Memgraph, AWS Neptune, etc.)?

Neo4j has the dominant graph database market share and the largest ecosystem (drivers, visualization
tools, training materials). Other graph DBs can be added later as separate crates if needed.

Note that Neo4j is the _most architecturally different_ of the three. It's not a SQL database, so:

- `SqlAccess` returns Unsupported (Neo4j backend isn't a SQL provider).
- Graph queries use the `khive-query` Cypher compiler (ADR-008).
- Vector storage may not be native — may require an external index (Qdrant, Weaviate) bridged via
  the trait.

### Why now (planning, not building)?

Building all three backends today would delay shipping v0.1. But the _crate structure_ must be right
from day one — renaming and refactoring after release breaks consumers.

By documenting the multi-backend plan in this ADR, we lock in the trait-based architecture without
paying for unbuilt backends. The `khive-db` crate name may be renamed to `khive-db-sqlite` when a
second backend lands, for symmetry.

## Alternatives Considered

| Alternative                                        | Pros                               | Cons                                           | Why rejected                              |
| -------------------------------------------------- | ---------------------------------- | ---------------------------------------------- | ----------------------------------------- |
| Feature flags on single `khive-db` crate           | Single crate to depend on          | Compile time, code complexity, mixed deps      | Worse for users                           |
| Backend-specific traits per crate                  | Each backend exposes its strengths | Loses swappability, services couple to backend | Defeats the purpose of trait-only storage |
| SQLite only (no portability)                       | Simplest                           | Locks out server deployments, no Neo4j interop | Strategically wrong                       |
| Separate "core" trait crate + "all backends" crate | Single dep                         | Same problems as feature flags                 | Worse abstraction                         |

## Consequences

### Positive

- Adding a new backend = adding a new crate. No changes to existing crates.
- Users opt in to only the backends they need.
- Independent release cadence per backend.
- Backend-specific bugs don't block other backends.

### Negative

- Three backends = three implementations to maintain. Mitigated: traits force consistent semantics;
  integration test suite runs against all backends.
- Migration tooling (e.g., schema migrations) needs per-backend logic. Mitigated:
  `ServiceSchemaPlan` allows separate SQLite and Postgres migration lists in the same plan.
- Trait abstraction means we can't expose backend-specific optimizations to consumers. Mitigated:
  backend-specific tuning lives inside the impl; the abstraction is a feature, not a bug.

### Neutral

- Documentation must clearly explain which backend to choose for which scenario.

## Common Trait Test Suite

Once `khive-db-postgres` ships, we'll need a conformance test suite that exercises every trait
method against every backend. The pattern:

```rust
async fn run_conformance<B: AllStores>(backend: B) { ... }

#[tokio::test] async fn test_sqlite() { run_conformance(SqliteBackend::memory().unwrap()).await }
#[tokio::test] async fn test_postgres() { run_conformance(PgBackend::test_db().await).await }
```

This guarantees behavioral equivalence and catches regressions where a backend diverges.

## Implementation Plan

### v0.1 (today)

- `khive-db` is the SQLite-backed implementation. The trait surface is the contract; the crate name
  is a label.

### v0.2

- Conformance test scaffold (single-backend for now, designed for multi).

### v0.3

- `khive-db-postgres` crate: implement all six traits against Postgres + pgvector + tsvector.
- Run conformance suite against Postgres.
- Document deployment differences (connection pooling, backup, migration runner).

### v0.4+

- `khive-db-neo4j`: implement non-SQL traits. `SqlAccess` returns Unsupported. Vector store bridge
  (or skip if user runs without semantic search).

## References

- ADR-005: Storage Capability Traits (the contracts every backend implements)
- ADR-008: Query Layer Separation (compilation targets per backend)
- `crates/khive-db/`: current SQLite implementation

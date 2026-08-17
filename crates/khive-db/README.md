# khive-db

SQLite storage backend for the khive knowledge graph runtime: entity, note, event,
edge, attachment, and content-addressed blob storage; FTS5 text search; and
optional `sqlite-vec` vector storage over a WAL-mode connection pool.

## Features

- **WAL-mode connection pool** — one writer, N concurrent readers (`ConnectionPool`)
- **Capability-trait factories** — `StorageBackend` hands out `Arc<dyn EntityStore>`,
  `GraphStore`, `NoteStore`, `EventStore`, `VectorStore`, `SparseStore`, `TextSearch`,
  `BlobStore`, `AttachmentStore`, and `SqlAccess` from `khive-storage`
- **Forward-only versioned migrations** — `run_migrations` applies `MIGRATIONS` in
  order, tracked in `_schema_migrations`
- **Legacy pack-scoped schema plans** — `ServiceSchemaPlan` / `apply_schema_plan` for
  pack-auxiliary tables tracked in `_schema_versions`
- **FTS5 trigram search** (CJK-safe) via `text()` / `text_with_tokenizer()`
- **`sqlite-vec` vector storage** (feature `vectors`) — per-model `vec0` virtual
  tables with a namespace-scoped embedding-model registry
- **Periodic WAL checkpoint task** (`checkpoint` module)

## Usage

```rust
use khive_db::{run_migrations, StorageBackend};

// File-backed (WAL mode, 1 writer + N readers) or StorageBackend::memory() for tests.
let backend = StorageBackend::sqlite("/path/to/khive.db")?;

{
    let mut writer = backend.pool().try_writer()?;
    run_migrations(writer.conn_mut())?;
}

let entities = backend.entities()?; // Arc<dyn khive_storage::EntityStore>
let attachments = backend.attachments()?; // Arc<dyn khive_storage::AttachmentStore>
let graph = backend.graph()?; // Arc<dyn khive_storage::GraphStore>
let text = backend.text("entities_fts")?; // Arc<dyn khive_storage::TextSearch>
let sql = backend.sql(); // Arc<dyn khive_storage::SqlAccess>, for pack-owned tables
```

Most legacy capability accessors (`entities`, `graph`, `notes`, `events`,
`vectors`, `sparse`,
`text`) apply their own DDL idempotently on first call. `attachments()` is the
exception: it never installs schema on demand because the coordinated V21
cutover owns table creation, GC fences, and legacy-column removal as one
boot-gated operation. Callers never need a separate "create schema" step per
store. Namespace-scoped variants
(`entities_for_namespace`, `graph_for_namespace`, …) validate that the namespace is
non-empty; the store itself remains namespace-agnostic — callers pass namespace on
each query.

## Migrations

Two migration systems coexist, both defined in `migrations.rs`:

- **Versioned** (`MIGRATIONS: &[VersionedMigration]`, applied by `run_migrations`) —
  the forward-only pipeline for core substrate tables (entities, notes, edges,
  events). `V1` is the consolidated fresh-start baseline loaded from
  `sql/schema.sql`; later versions are incremental `.sql` files applied in order and
  tracked in `_schema_migrations`. A database whose recorded version is ahead of the
  latest known migration fails loudly rather than silently skipping the baseline.
- **Legacy per-service** (`ServiceSchemaPlan` / `apply_schema_plan`) — used by packs
  that declare their own auxiliary DDL, tracked per-service in `_schema_versions`.

Schema DDL is authored in `crates/khive-db/sql/*.sql` and pulled in via
`include_str!` — never hand-written as inline Rust string literals. Adding a
migration means a new `.sql` file plus a new `VersionedMigration` entry; `V1` itself
is never edited on an existing database.

V21 is a coordinated Phase-4b exception to ordinary eager application. A
zero-reference database may complete it atomically inside `run_migrations`; a
legacy V20 database stops at V20 and requires the async MCP/kkernel host
coordinator to hold the blob-GC owner, stage attachments, authenticate pack-owned
roles, and finalize. The V21 ledger row is written only in that final transaction.
The coordinator then releases GC ownership and applies ordinary V22
(`embedding_space_shadow_stage`) before returning. Low-level prepared-runtime
construction requires exact current V22; a completed V21 database is physically
valid attachment history but is behind this binary.

V22 creates only `_embedding_models_v22_shadow`,
`_embedding_model_legacy_provenance`, and `_embedding_space_cutover_state`. It
records exact legacy tuple provenance and `legacy_staged` state while the old
`_embedding_models` table remains authoritative. It does not switch providers,
vector/ANN/cache/log identity, copy vectors, or complete ADR-160 Phase 7.

Phase 4b may run only after the separately shipped Phase-4a transactional-GC
epoch gate has converged across every process sharing the database/blob root and
all pre-Phase-4a processes have been drained. Phase 4a leaves V20 untouched and
refuses transactional GC on V20 or any incomplete/malformed V21 state in both
dry-run and destructive modes; it does not create or backfill attachments.
Before cutover, also quiesce every Phase-4a application reader/writer or prove
it cannot access the database. Only a GC-only worker has narrow completed-V21
compatibility; start Phase-4b serving after exact-current topology validation.
The current filesystem GC gate admits only V21 or canonical
`(22, "embedding_space_shadow_stage")` after revalidating the complete V21
physical fence. Foreign V22 and unknown V23-or-later schemas fail closed before
root/filesystem work; an older exact-V21 Phase-4a binary also
safely refuses V22 as ahead of its contract.

## Vector storage

`vectors_for_namespace(model_key, embedding_model, dimensions, namespace)` creates a
`vec_<model_key>` virtual table (via `sqlite-vec`, feature `vectors`) sized to
`dimensions`, with cosine distance. `model_key` must be ASCII
alphanumeric/underscore. Tables predating the `field`/`embedding_model` columns
(pre-v0.2.8) are rejected with an explicit error rather than silently dropped —
vector data is a cache, so callers re-embed after recreating the table.

## Where this sits

`khive-db` implements the `khive-storage` capability traits
([ADR-005](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-005-storage-capability-traits.md))
against SQLite, sitting directly above `khive-storage`/`khive-score`/`khive-types` and
below `khive-query` and `khive-runtime` in the storage dependency chain:

```text
types -> score -> storage -> db -> query -> runtime -> pack-* -> mcp
```

`khive-runtime`'s `KhiveRuntime` wraps `StorageBackend` and layers namespace
authorization and pack dispatch on top. Schema evolution follows
[ADR-015](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-015-schema-migrations.md).

## License

Apache-2.0.

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
use khive_db::StorageBackend;

// File-backed (WAL mode, 1 writer + N readers) or StorageBackend::memory() for tests.
let backend = StorageBackend::sqlite("/path/to/khive.db")?;

// Core migrations through the pool's own write admission. The raw
// `run_migrations(conn)` takes the volume lease itself, so it is not called on a
// connection borrowed from a pool writer that is still held.
backend.pool().run_migrations()?;

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

## Filtered note reads and guarded attachment cleanup

`NoteFilter::created_before(micros)` and `expires_before(micros)` add inclusive
upper bounds (`<=`) measured in microseconds since the Unix epoch. An expiry
bound excludes notes whose `expires_at` is absent. Bounds compose with the
existing namespace, property and minimum-creation selectors and apply to filtered
pages, counts and cursors. Defaults remain live-only and unbounded.

`NoteFilter::include_deleted()` includes tombstones in those reads. It does not
relax the live-row precondition of either scalar or atomic property patches.
Tombstone reads do not pin indexes whose predicates require live notes.
Both filtered patch APIs also validate each JSON filter path before writer
admission; malformed paths refuse without changing any target.

`NoteFilter::expiry_fallback` accepts a `NoteExpiryFallback` with inclusive
`expires_at_or_before` and `created_at_or_before` bounds. It matches an expiry at
or before the first bound, or an absent expiry with creation at or before the
second. A future expiry never falls back to old creation time. Other bounds
remain additional AND constraints.

For a bounded cleanup page, `time_order` accepts `NoteTimeOrder::ExpiresAt` or
`ExpiresAtOrCreatedAt`: ascending expiry or `COALESCE(expires_at, created_at)`,
then ascending ID. Counted, count-free and bounded pages apply this order before
LIMIT. Counts ignore ordering; keyed/sequence reads and filtered mutations
reject it. It cannot be combined with property/instant/unordered ordering or
cursor fields. Existing ordering remains unchanged when it is absent.

Two generic property operators preserve legacy JSON selectors:
`MissingNullOrSpaceEmptyText` matches missing/null values or text empty after
SQLite's ASCII-space-only trim; tabs and Unicode whitespace do not match.
`TrueOrTextTrue` matches JSON boolean true or exact text `"true"`, excluding
numeric 1. Their `PropertyFilter.value` is unused. These predicates run in SQL
before ordering and LIMIT, so rejected rows cannot starve a bounded page.

`AttachmentStore::delete_attachment_if(id, role, substrate, &content_ref)` removes
one attachment only while all four values match in the same conditional write.
It returns `false` for a missing or changed attachment, leaves other roles and
owners untouched, and does not delete blob content. SQLite uses its existing
writer admission and queue routes; backends without this primitive return
`Unsupported` instead of using a read followed by an unguarded delete.

These are storage foundations for comm cleanup; comm consumers still use their
existing queries and cleanup policy.

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

Phase 4b may run only after the separately shipped Phase-4a transactional-GC
epoch gate has converged across every process sharing the database/blob root and
all pre-Phase-4a processes have been drained. Phase 4a leaves V20 untouched and
refuses transactional GC on V20 or any incomplete/malformed V21 state in both
dry-run and destructive modes; it does not create or backfill attachments.
Before cutover, also quiesce every Phase-4a application reader/writer or prove
it cannot access the database. Only a GC-only worker has narrow completed-V21
compatibility; start Phase-4b serving after exact-current topology validation.

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

# Persistence Layer

khive-db is the SQLite storage backend for the khive knowledge graph runtime.
It implements the capability traits defined in `khive-storage` and provides
the concrete persistence for all data substrates.

## What it stores

| Substrate | Table         | Store module        | Capability trait |
| --------- | ------------- | ------------------- | ---------------- |
| Entities  | `entities`    | `stores/entity.rs`  | `EntityStore`    |
| Notes     | `notes`       | `stores/note.rs`    | `NoteStore`      |
| Edges     | `graph_edges` | `stores/graph.rs`   | `GraphStore`     |
| Events    | `events`      | `stores/event.rs`   | `EventStore`     |
| Vectors   | vec0 virtual  | `stores/vectors.rs` | `VectorStore`    |
| FTS index | FTS5 virtual  | `stores/text.rs`    | `TextSearch`     |
| Sparse    | --            | `stores/sparse.rs`  | `SparseStore`    |

## StorageBackend

`backend.rs` owns the `ConnectionPool` and exposes factory methods for each
store. Two modes:

- **File-backed** (`StorageBackend::sqlite(path)`) -- WAL mode, 1 writer + N
  readers for concurrent access. Used in production.
- **In-memory** (`StorageBackend::memory()`) -- single-connection mode. Used
  in tests.

The backend also provides:

- `apply_schema(plan)` -- run legacy service-level migrations
- `apply_pack_ddl_statements(stmts)` -- run pack-auxiliary DDL (ADR-017)
- `sql()` -- raw `SqlAccess` bridge for the query compiler

## Connection pooling

`pool.rs` manages a writer lock (exclusive) and reader connections. All store
operations acquire connections via `spawn_blocking` to avoid blocking the
async runtime.

## Schema management

See [migration.md](migration.md) for the versioned migration system. Store
DDL constants (`ENTITIES_DDL`, `NOTES_DDL`, etc.) are used for in-process
schema creation in tests and include all columns from the latest migration
version.

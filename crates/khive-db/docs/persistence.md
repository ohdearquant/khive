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

`SqlNoteStore::set_note_property` applies one top-level JSON-key update with a
single parameterized SQLite `json_set` statement. This is the note capability's
atomic patch path; whole-document replacement remains a separate explicit
operation. Keys containing U+0000 are rejected before dispatch because SQLite
JSON paths cannot address those labels without ambiguous prefix matching.

Each pack DDL plan is applied atomically in one transaction and remains idempotent on repeat application.

## Audit unreadable event profile versions

Run the [read-only event profile version audit](api/event-profile-state-version-audit.sql)
on demand against the database with a read-only SQLite connection, for example:

```sh
sqlite3 -readonly /absolute/path/to/khive.db \
  < crates/khive-db/docs/api/event-profile-state-version-audit.sql
```

Each result identifies an event ID, namespace, SQLite storage type, and quoted
stored value. The query finds every non-null `profile_state_version` that the
event reader cannot decode: a non-integer storage type or a negative integer.
An empty result means this specific column has no unreadable rows. This is an
explicit full-table audit; normal `db_diagnostics` calls do not run it.

There is no automatic repair or quarantine. Events are immutable audit and
replay records ([ADR-004](../../../docs/adr/ADR-004-substrate-observables.md)),
and an invalid stored value does not reveal the version originally intended.
Coercing it to zero, null, or a guessed value would fabricate history; removing
the row could also break replay and its `event_observations` projection. Preserve
a database copy and investigate the listed IDs and their original source before
any operator-led recovery decision.

## Connection pooling

`pool.rs` manages a writer lock (exclusive) and reader connections. All store
operations acquire connections via `spawn_blocking` to avoid blocking the
async runtime.

## Schema management

See [migration.md](migration.md) for the versioned migration system. Store
DDL constants (`ENTITIES_DDL`, `NOTES_DDL`, etc.) are used for in-process
schema creation in tests and include all columns from the latest migration
version.

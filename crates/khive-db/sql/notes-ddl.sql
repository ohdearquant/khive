-- Notes table and supporting indexes.
-- Applied idempotently by StorageBackend::notes_for_namespace on every store access.
-- Cheap on every call (CREATE ... IF NOT EXISTS is a catalog lookup, not a
-- table scan) -- unlike the notes_seq repair, which is gated separately
-- (see `stores/note.rs::repair_notes_seq` and `StorageBackend::notes_for_namespace`).

CREATE TABLE IF NOT EXISTS notes (
    id           TEXT PRIMARY KEY,
    namespace    TEXT NOT NULL,
    kind         TEXT NOT NULL,
    status       TEXT NOT NULL DEFAULT 'active',
    name         TEXT,
    content      TEXT NOT NULL DEFAULT '',
    salience     REAL,
    decay_factor REAL,
    expires_at   INTEGER,
    properties   TEXT,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    deleted_at   INTEGER
);

CREATE INDEX IF NOT EXISTS idx_notes_namespace ON notes(namespace);
CREATE INDEX IF NOT EXISTS idx_notes_kind ON notes(namespace, kind);
CREATE INDEX IF NOT EXISTS idx_notes_created ON notes(created_at DESC);

-- Durable, non-reusing sequence for notes (khive #827). Kept in sync with
-- `sql/007-notes-seq.sql` (the versioned-migration copy) — see that file for
-- the full rationale. Duplicated here, belt-and-suspenders style, because
-- this DDL is applied lazily on every `notes_for_namespace` call, independent
-- of whether `run_migrations` has run against this connection.
CREATE TABLE IF NOT EXISTS notes_seq (
    seq     INTEGER PRIMARY KEY AUTOINCREMENT,
    note_id TEXT NOT NULL UNIQUE
);

CREATE INDEX IF NOT EXISTS idx_notes_seq_note_id ON notes_seq(note_id);

-- The notes_seq anti-join repair (khive #827 round 3) used to run here, on
-- every `notes_for_namespace` call. On a large, already-repaired ledger that
-- is a full `notes` scan plus a temp B-tree for the ORDER BY on every single
-- store acquisition, serializing every caller behind the writer mutex for no
-- benefit once the ledger has nothing left to repair (khive #827 round 4).
-- The repair itself now lives in `stores/note.rs::repair_notes_seq` (still
-- sourced from `sql/008-notes-seq-repair.sql`, same anti-join) and is invoked
-- by `StorageBackend::notes_for_namespace`, gated to run at most once per
-- backend/pool for the process's lifetime via an atomic counter on
-- `StorageBackend`.

-- Notes table and supporting indexes.
-- Applied idempotently by StorageBackend::notes_for_namespace on every store access.

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

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

-- Backfill, mirroring `sql/007-notes-seq.sql` (khive #827) -- see that file
-- for the full rationale. This DDL runs on every `notes_for_namespace` call
-- (not just once, like the versioned migration), including for callers that
-- build a store directly and never call `run_migrations`. Without this, a
-- populated database opened only through this lazy path would keep every
-- pre-existing note permanently invisible to `comm.probe`. The `WHERE NOT
-- EXISTS` guard keeps this a one-time cost per database: once any row is
-- present in `notes_seq` -- either from this backfill or from the first
-- real insert going through `assign_note_seq` -- every later call skips the
-- full-table scan.
INSERT OR IGNORE INTO notes_seq (note_id)
SELECT id FROM notes
WHERE NOT EXISTS (SELECT 1 FROM notes_seq)
ORDER BY created_at ASC, id ASC;

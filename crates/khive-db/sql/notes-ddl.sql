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

-- Repair, mirroring `sql/008-notes-seq-repair.sql` (khive #827 round 3) --
-- see that file for the full rationale. This DDL runs on every
-- `notes_for_namespace` call (not just once, like the versioned migration),
-- including for callers that build a store directly and never call
-- `run_migrations`. It targets notes MISSING a `notes_seq` row specifically
-- -- via an anti-join, not "the table is globally empty" -- because a
-- database can have a partially populated `notes_seq` (e.g. one post-V7
-- note landed through `assign_note_seq` before this lazy path ever ran):
-- checking global emptiness alone would see that one row and skip repairing
-- every older, still-unmapped note, permanently excluding them from
-- `comm.probe`. `INSERT OR IGNORE` makes this idempotent and cheap once the
-- ledger is fully populated -- the anti-join still scans `notes`, but every
-- row it finds is already excluded by `idx_notes_seq_note_id`.
INSERT OR IGNORE INTO notes_seq (note_id)
SELECT n.id FROM notes n
WHERE NOT EXISTS (SELECT 1 FROM notes_seq s WHERE s.note_id = n.id)
ORDER BY n.created_at ASC, n.id ASC;

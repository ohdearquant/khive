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

-- Partial index for the unread-message probe (comm unread badge + inbox
-- unread listing). Its WHERE clause is the exact predicate the
-- JsonTypeNeMissing filter op generates (with the json_type value inlined
-- as a literal -- a bound parameter cannot prove implication at plan time),
-- so the planner serves unread scans from only the unread rows: work is
-- proportional to min(unread, scan cap), never to total mailbox size.
CREATE INDEX IF NOT EXISTS idx_notes_unread_probe
    ON notes(namespace, kind, created_at DESC)
    WHERE (json_type(properties, '$.read') IS NULL
           OR json_type(properties, '$.read') != 'true')
      AND deleted_at IS NULL;

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

-- Migration V13 adds this trigger to migrated databases. Keep the idempotent
-- mirror here so a fresh/direct store built from NOTES_DDL also assigns every
-- new note its immutable list sequence atomically. The existing explicit
-- `assign_note_seq` calls remain harmless INSERT OR IGNORE safeguards.
CREATE TRIGGER IF NOT EXISTS assign_note_list_seq
AFTER INSERT ON notes
BEGIN
    -- Explicit UPSERT keeps the ledger immutable even when an outer caller
    -- uses `INSERT OR REPLACE` (which overrides legacy trigger OR policies).
    INSERT INTO notes_seq (note_id) VALUES (NEW.id)
    ON CONFLICT(note_id) DO NOTHING;
END;

-- The notes_seq anti-join repair (khive #827) used to run here, on
-- every `notes_for_namespace` call. On a large, already-repaired ledger that
-- is a full `notes` scan plus a temp B-tree for the ORDER BY on every single
-- store acquisition, serializing every caller behind the writer mutex for no
-- benefit once the ledger has nothing left to repair (khive #827).
-- The repair itself now lives in `stores/note.rs::repair_notes_seq` (still
-- sourced from `sql/008-notes-seq-repair.sql`, same anti-join) and is invoked
-- by `StorageBackend::notes_for_namespace`, gated to run at most once per
-- backend/pool for the process's lifetime via an atomic counter on
-- `StorageBackend`.

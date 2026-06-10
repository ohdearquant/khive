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

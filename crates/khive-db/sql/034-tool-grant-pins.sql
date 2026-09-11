-- The tool pack may never have been loaded in this database.
CREATE TABLE IF NOT EXISTS tool_grants (
    id TEXT PRIMARY KEY,
    namespace TEXT NOT NULL,
    actor TEXT NOT NULL,
    tool TEXT NOT NULL,
    scope TEXT,
    reason TEXT,
    status TEXT NOT NULL,
    requested_at INTEGER NOT NULL,
    decided_at INTEGER,
    decided_by TEXT,
    expires_at INTEGER,
    decision_note TEXT
);

-- Legacy decisions carry no evidence of which definition was approved.
ALTER TABLE tool_grants ADD COLUMN registry_id TEXT;
ALTER TABLE tool_grants ADD COLUMN definition_digest TEXT;
ALTER TABLE tool_grants ADD COLUMN invalidated_by_registry_id TEXT;
ALTER TABLE tool_grants ADD COLUMN invalidated_at INTEGER;

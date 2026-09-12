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
    decision_note TEXT,
    registry_id TEXT,
    definition_digest TEXT,
    invalidated_by_registry_id TEXT,
    invalidated_at INTEGER
);

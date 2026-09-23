CREATE TABLE IF NOT EXISTS tool_policy (
    id TEXT PRIMARY KEY,
    namespace TEXT NOT NULL,
    actor TEXT NOT NULL,
    tool TEXT NOT NULL,
    decision TEXT NOT NULL,
    note TEXT,
    created_at INTEGER NOT NULL,
    created_by TEXT,
    updated_at INTEGER,
    updated_by TEXT,
    history TEXT,
    deleted_at INTEGER,
    deleted_by TEXT
);

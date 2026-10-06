-- Preparation schema for kkernel queries against the legacy snapshot table.
-- Not a migration: scripts/lint-sql.sh loads this fixture to resolve the table
-- and column names used by the deletion statements in this directory.
-- Keep this schema compatible with those statements; it does not initialize
-- or own a runtime snapshot store.
CREATE TABLE IF NOT EXISTS retrieval_snapshots (
    namespace TEXT NOT NULL,
    index_type TEXT NOT NULL,
    snapshot BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (namespace, index_type)
)

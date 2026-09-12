-- Preparation schema for kkernel queries against the runtime-created snapshot store.
CREATE TABLE IF NOT EXISTS retrieval_snapshots (
    namespace TEXT NOT NULL,
    index_type TEXT NOT NULL,
    snapshot BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (namespace, index_type)
)

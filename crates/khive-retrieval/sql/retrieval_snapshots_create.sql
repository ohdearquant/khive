CREATE TABLE IF NOT EXISTS retrieval_snapshots (
    namespace   TEXT NOT NULL,
    index_type  TEXT NOT NULL,
    snapshot    BLOB NOT NULL,
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (namespace, index_type)
);

CREATE INDEX IF NOT EXISTS idx_retrieval_snapshots_namespace
    ON retrieval_snapshots(namespace);

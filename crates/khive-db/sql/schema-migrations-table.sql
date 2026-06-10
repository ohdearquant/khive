-- Internal tracking table for the versioned migration system.
-- Applied once by run_migrations() before processing any migration.

CREATE TABLE IF NOT EXISTS _schema_migrations (
    version    INTEGER PRIMARY KEY,
    name       TEXT NOT NULL,
    applied_at INTEGER NOT NULL
);

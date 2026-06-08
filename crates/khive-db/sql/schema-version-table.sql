-- Internal tracking table for the legacy per-service migration system.
-- Applied idempotently by apply_schema_plan() before each schema plan run.

CREATE TABLE IF NOT EXISTS _schema_versions (
    service        TEXT NOT NULL,
    migration_id   TEXT NOT NULL,
    applied_at     INTEGER NOT NULL,
    PRIMARY KEY (service, migration_id)
);

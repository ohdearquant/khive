-- Embedding model registry table and supporting indexes.
-- Used as a belt-and-suspenders creation in StorageBackend::vectors_for_namespace
-- and register_embedding_model, in addition to the V1 schema migration.

CREATE TABLE IF NOT EXISTS _embedding_models (
    id              BLOB PRIMARY KEY,
    engine_name     TEXT NOT NULL,
    model_id        TEXT NOT NULL,
    key_version     TEXT NOT NULL,
    dim             INTEGER NOT NULL,
    output_dim      INTEGER,
    status          TEXT NOT NULL CHECK (status IN ('pending', 'active', 'superseded', 'archived')),
    activated_at    INTEGER,
    superseded_at   INTEGER,
    superseded_by   BLOB,
    canonical_key   BLOB NOT NULL UNIQUE,
    created_at      INTEGER NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_embed_models_one_active
    ON _embedding_models(engine_name) WHERE status = 'active';

CREATE INDEX IF NOT EXISTS idx_embed_models_engine_status
    ON _embedding_models(engine_name, status);

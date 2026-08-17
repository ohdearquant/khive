-- V22 stage 1: dormant embedding-space registry shadow (ADR-160 D6).
--
-- The canonical _embedding_models registry remains authoritative until a
-- later, coordinated rebuild verifies every replacement space and completes
-- the atomic cutover.  These objects deliberately use fail-closed creation:
-- replaying the stage DDL against a partial or pre-existing shadow is an error.

CREATE TABLE _embedding_models_v22_shadow (
    id                      BLOB PRIMARY KEY NOT NULL,
    lineage_slot            TEXT NOT NULL,
    space_key               TEXT NOT NULL UNIQUE,
    identity_protocol       TEXT NOT NULL,
    identity_fingerprint    BLOB NOT NULL
        CHECK (
            typeof(identity_fingerprint) = 'blob'
            AND length(identity_fingerprint) = 32
        ),
    model_name              TEXT NOT NULL,
    dimensions              INTEGER NOT NULL
        CHECK (
            typeof(dimensions) = 'integer'
            AND dimensions BETWEEN 1 AND 8192
        ),
    status                  TEXT NOT NULL
        CHECK (status IN ('pending', 'active', 'superseded', 'archived')),
    activated_at            INTEGER,
    superseded_at           INTEGER,
    superseded_by           BLOB,
    created_at              INTEGER NOT NULL
) STRICT;

CREATE UNIQUE INDEX idx_embedding_models_v22_shadow_one_active
    ON _embedding_models_v22_shadow(lineage_slot)
    WHERE status = 'active';

CREATE INDEX idx_embedding_models_v22_shadow_lineage_status
    ON _embedding_models_v22_shadow(lineage_slot, status);

CREATE TABLE _embedding_model_legacy_provenance (
    id                      BLOB PRIMARY KEY NOT NULL,
    engine_name             TEXT NOT NULL,
    model_id                TEXT NOT NULL,
    key_version             TEXT NOT NULL,
    dim                     INTEGER NOT NULL,
    output_dim              INTEGER,
    canonical_key           BLOB NOT NULL,
    pre_migration_status    TEXT NOT NULL
        CHECK (pre_migration_status IN ('pending', 'active', 'superseded', 'archived'))
) STRICT;

CREATE TABLE _embedding_space_cutover_state (
    singleton       INTEGER PRIMARY KEY CHECK (singleton = 1),
    state           TEXT NOT NULL
        CHECK (state IN ('unstaged', 'legacy_staged', 'rebuild_ready', 'complete')),
    staged_at       INTEGER,
    completed_at    INTEGER,
    CHECK (
        (state = 'unstaged' AND staged_at IS NULL AND completed_at IS NULL)
        OR (state = 'legacy_staged' AND staged_at IS NOT NULL AND completed_at IS NULL)
        OR (state = 'rebuild_ready' AND staged_at IS NOT NULL AND completed_at IS NULL)
        OR (state = 'complete' AND staged_at IS NOT NULL AND completed_at IS NOT NULL)
    )
) STRICT;

INSERT INTO _embedding_space_cutover_state (
    singleton,
    state,
    staged_at,
    completed_at
) VALUES (1, 'unstaged', NULL, NULL);

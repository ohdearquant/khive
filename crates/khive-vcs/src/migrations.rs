// Copyright 2026 khive contributors. Licensed under Apache-2.0.
//
//! SQL migrations for the VCS layer (ADR-042 §2).
//!
//! These migrations create the `kg_snapshots`, `kg_snapshot_archives`,
//! `kg_branches`, and `kg_vcs_state` tables. They follow the `ServiceSchemaPlan`
//! pattern established in ADR-022.

/// SQL DDL that creates all VCS tables.
///
/// Designed to be run inside `khive-db`'s migration framework. Safe to call
/// on a database that already has these tables (all statements use `CREATE TABLE IF NOT EXISTS`).
pub const VCS_MIGRATIONS_V1: &str = r#"
CREATE TABLE IF NOT EXISTS kg_snapshots (
    id           TEXT    PRIMARY KEY,
    namespace    TEXT    NOT NULL,
    parent_id    TEXT    REFERENCES kg_snapshots(id),
    message      TEXT    NOT NULL DEFAULT '',
    author       TEXT,
    created_at   INTEGER NOT NULL,
    entity_count INTEGER NOT NULL DEFAULT 0,
    edge_count   INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS kg_snapshot_archives (
    snapshot_id  TEXT    PRIMARY KEY REFERENCES kg_snapshots(id) ON DELETE CASCADE,
    archive_json TEXT    NOT NULL,
    format       TEXT    NOT NULL DEFAULT 'full'
);

CREATE TABLE IF NOT EXISTS kg_branches (
    namespace    TEXT    NOT NULL,
    name         TEXT    NOT NULL,
    head_id      TEXT    NOT NULL REFERENCES kg_snapshots(id),
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    PRIMARY KEY (namespace, name)
);

CREATE TABLE IF NOT EXISTS kg_vcs_state (
    namespace          TEXT    PRIMARY KEY,
    current_branch     TEXT,
    last_committed_id  TEXT    REFERENCES kg_snapshots(id),
    dirty              INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_snapshots_ns_created
    ON kg_snapshots(namespace, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_snapshots_parent
    ON kg_snapshots(parent_id)
    WHERE parent_id IS NOT NULL;
"#;

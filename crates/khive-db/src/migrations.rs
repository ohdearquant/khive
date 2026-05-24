use rusqlite::Connection;

use crate::error::SqliteError;

// =============================================================================
// Legacy per-service migration API (preserved for backward compatibility)
// =============================================================================

pub struct Migration {
    pub id: &'static str,
    pub up_sql: &'static str,
    pub down_sql: Option<&'static str>,
    pub is_already_applied: Option<fn(&Connection) -> bool>,
}

pub struct ServiceSchemaPlan {
    pub service: &'static str,
    pub sqlite: &'static [Migration],
    pub postgres: &'static [Migration],
}

const SCHEMA_VERSION_TABLE: &str = "\
    CREATE TABLE IF NOT EXISTS _schema_versions (\
        service TEXT NOT NULL,\
        migration_id TEXT NOT NULL,\
        applied_at INTEGER NOT NULL,\
        PRIMARY KEY (service, migration_id)\
    );\
";

pub fn apply_schema_plan(conn: &Connection, plan: &ServiceSchemaPlan) -> Result<(), SqliteError> {
    conn.execute_batch(SCHEMA_VERSION_TABLE)?;

    for migration in plan.sqlite {
        // Check if custom predicate says it's already applied
        if let Some(check) = migration.is_already_applied {
            if check(conn) {
                continue;
            }
        }

        // Check if tracked as applied
        let already: bool = conn.query_row(
            "SELECT COUNT(*) > 0 FROM _schema_versions WHERE service = ?1 AND migration_id = ?2",
            rusqlite::params![plan.service, migration.id],
            |row| row.get(0),
        )?;

        if already {
            continue;
        }

        // Apply
        conn.execute_batch(migration.up_sql)?;

        // Record
        conn.execute(
            "INSERT INTO _schema_versions (service, migration_id, applied_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                plan.service,
                migration.id,
                chrono::Utc::now().timestamp_micros(),
            ],
        )?;
    }

    Ok(())
}

// =============================================================================
// Versioned migration system (ADR-022)
// =============================================================================

/// A single forward-only schema migration.
///
/// Migrations are applied in order from the current DB version to the target
/// version. Each migration runs in its own transaction; a failure rolls back
/// that migration and leaves the DB at the prior version.
pub struct VersionedMigration {
    /// Monotonically increasing version number, starting at 1.
    pub version: u32,
    /// Short human-readable name for the migration (used in the audit table).
    pub name: &'static str,
    /// SQL to apply this migration. May contain multiple statements separated
    /// by semicolons; `execute_batch` runs them all.
    pub up: &'static str,
}

// V1: The complete initial schema for all four core tables.
const V1_UP: &str = "\
    CREATE TABLE IF NOT EXISTS entities (\
        id TEXT PRIMARY KEY,\
        namespace TEXT NOT NULL,\
        kind TEXT NOT NULL,\
        name TEXT NOT NULL,\
        description TEXT,\
        properties TEXT,\
        tags TEXT NOT NULL DEFAULT '[]',\
        created_at INTEGER NOT NULL,\
        updated_at INTEGER NOT NULL,\
        deleted_at INTEGER\
    );\
    CREATE INDEX IF NOT EXISTS idx_entities_namespace ON entities(namespace);\
    CREATE INDEX IF NOT EXISTS idx_entities_kind ON entities(namespace, kind);\
    CREATE INDEX IF NOT EXISTS idx_entities_name ON entities(namespace, name);\
    CREATE INDEX IF NOT EXISTS idx_entities_created ON entities(created_at DESC);\
    CREATE TABLE IF NOT EXISTS graph_edges (\
        namespace TEXT NOT NULL,\
        id TEXT NOT NULL,\
        source_id TEXT NOT NULL,\
        target_id TEXT NOT NULL,\
        relation TEXT NOT NULL,\
        weight REAL NOT NULL DEFAULT 1.0,\
        created_at INTEGER NOT NULL,\
        metadata TEXT,\
        PRIMARY KEY (namespace, id)\
    );\
    CREATE INDEX IF NOT EXISTS idx_graph_edges_ns_source ON graph_edges(namespace, source_id);\
    CREATE INDEX IF NOT EXISTS idx_graph_edges_ns_target ON graph_edges(namespace, target_id);\
    CREATE INDEX IF NOT EXISTS idx_graph_edges_ns_relation ON graph_edges(namespace, relation);\
    CREATE INDEX IF NOT EXISTS idx_graph_edges_ns_src_rel ON graph_edges(namespace, source_id, relation);\
    CREATE INDEX IF NOT EXISTS idx_graph_edges_ns_tgt_rel ON graph_edges(namespace, target_id, relation);\
    CREATE TABLE IF NOT EXISTS notes (\
        id TEXT PRIMARY KEY,\
        namespace TEXT NOT NULL,\
        kind TEXT NOT NULL,\
        content TEXT NOT NULL DEFAULT '',\
        salience REAL NOT NULL DEFAULT 0.5,\
        decay_factor REAL NOT NULL DEFAULT 0.0,\
        expires_at INTEGER,\
        properties TEXT,\
        created_at INTEGER NOT NULL,\
        updated_at INTEGER NOT NULL,\
        deleted_at INTEGER\
    );\
    CREATE INDEX IF NOT EXISTS idx_notes_namespace ON notes(namespace);\
    CREATE INDEX IF NOT EXISTS idx_notes_kind ON notes(namespace, kind);\
    CREATE INDEX IF NOT EXISTS idx_notes_created ON notes(created_at DESC);\
    CREATE TABLE IF NOT EXISTS events (\
        id TEXT PRIMARY KEY,\
        namespace TEXT NOT NULL,\
        verb TEXT NOT NULL,\
        substrate TEXT NOT NULL,\
        actor TEXT NOT NULL,\
        outcome TEXT NOT NULL,\
        data TEXT,\
        duration_us INTEGER NOT NULL DEFAULT 0,\
        target_id TEXT,\
        created_at INTEGER NOT NULL\
    );\
    CREATE INDEX IF NOT EXISTS idx_events_namespace ON events(namespace);\
    CREATE INDEX IF NOT EXISTS idx_events_verb ON events(verb);\
    CREATE INDEX IF NOT EXISTS idx_events_substrate ON events(substrate);\
    CREATE INDEX IF NOT EXISTS idx_events_created ON events(created_at DESC);\
";

/// All known migrations, ordered by ascending version.
///
/// To add a new migration: append a `VersionedMigration` entry with
/// `version = <last_version + 1>`. The version sequence must be contiguous
/// (1, 2, 3, ...); `run_migrations` returns an error on gaps.
///
/// V2 note: `NOTES_DDL` in `stores/note.rs` already includes `name TEXT` so that
/// in-process schema creation (used by tests and `StorageBackend::notes()`) has the
/// column from the start.  When `run_migrations` is called on a DB that was
/// bootstrapped via `NOTES_DDL`, the V2 `ALTER TABLE` would fail with "duplicate
/// column name".  The migration runner handles this by checking column existence
/// before applying V2 — see `run_migrations`.
///
/// V4 note: Deduplicates existing graph_edges rows that share the same
/// (namespace, source_id, target_id, relation) triple, keeping the earliest
/// rowid, then adds a unique index enforcing the constraint going forward.
const V4_DEDUPE_GRAPH_EDGE_TRIPLES: &str = "\
    DELETE FROM graph_edges \
    WHERE rowid NOT IN (\
        SELECT MIN(rowid) \
        FROM graph_edges \
        GROUP BY namespace, source_id, target_id, relation\
    );\
    CREATE UNIQUE INDEX IF NOT EXISTS idx_graph_edges_unique_triple \
    ON graph_edges(namespace, source_id, target_id, relation);\
";

// V5 adds event observability + provenance columns and the event_observations table.
// The DDL is computed at runtime via `build_v5_event_observability_sql` so that
// running migrations on a database already bootstrapped by `ensure_events_schema`
// (which includes the new columns) does not fail with "duplicate column name".
const V5_EVENT_OBSERVABILITY_PROVENANCE: &str = "__v5_computed_at_runtime__";

pub const MIGRATIONS: &[VersionedMigration] = &[
    VersionedMigration {
        version: 1,
        name: "initial_schema",
        up: V1_UP,
    },
    VersionedMigration {
        version: 2,
        name: "add_name_to_notes",
        up: "ALTER TABLE notes ADD COLUMN name TEXT;",
    },
    VersionedMigration {
        version: 3,
        name: "add_events_namespace_created_index",
        up: "CREATE INDEX IF NOT EXISTS idx_events_ns_created ON events(namespace, created_at DESC);",
    },
    VersionedMigration {
        version: 4,
        name: "dedupe_graph_edge_triples",
        up: V4_DEDUPE_GRAPH_EDGE_TRIPLES,
    },
    VersionedMigration {
        version: 5,
        name: "event_observability_provenance",
        up: V5_EVENT_OBSERVABILITY_PROVENANCE,
    },
];

const MIGRATION_TRACKING_TABLE: &str = "\
    CREATE TABLE IF NOT EXISTS _schema_migrations (\
        version   INTEGER PRIMARY KEY,\
        name      TEXT NOT NULL,\
        applied_at INTEGER NOT NULL\
    );\
";

/// Apply all unapplied migrations from `MIGRATIONS` in order.
///
/// Returns the highest version now applied, or `0` if the DB is empty and no
/// migrations exist.
///
/// # Idempotency
///
/// Safe to call multiple times. Already-applied migrations are skipped.
///
/// # Atomicity
///
/// Each migration runs in its own transaction. A failure rolls back that
/// migration and leaves the DB at the prior version.
///
/// # Errors
///
/// Returns `SqliteError::InvalidData` if the `MIGRATIONS` array is not
/// contiguous (1, 2, 3, ...).
///
/// Returns `SqliteError::Migration { version, error }` if any migration fails.
pub fn run_migrations(conn: &mut Connection) -> Result<u32, SqliteError> {
    for (i, m) in MIGRATIONS.iter().enumerate() {
        let expected = (i + 1) as u32;
        if m.version != expected {
            return Err(SqliteError::InvalidData(format!(
                "MIGRATIONS array is not contiguous: expected version {expected} at index {i}, \
                 got version {}",
                m.version
            )));
        }
    }

    conn.execute_batch(MIGRATION_TRACKING_TABLE)?;

    // Determine the current version (highest applied).
    let current_version: u32 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM _schema_migrations",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    let mut applied_version = current_version;

    for migration in MIGRATIONS {
        if migration.version <= current_version {
            continue;
        }

        // V2 adds `name` to notes.  StorageBackend::notes() bootstraps the schema
        // via NOTES_DDL (which already includes `name`), so the column may already
        // exist even though the migration has never been recorded.  Treat "duplicate
        // column name" from SQLite as idempotent for ALTER TABLE migrations.
        if migration.version == 2 {
            let col_exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM pragma_table_info('notes') WHERE name = 'name'",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            if col_exists {
                // Column already present — record the migration as applied and skip.
                let now = chrono::Utc::now().timestamp_micros();
                conn.execute(
                    "INSERT OR IGNORE INTO _schema_migrations (version, name, applied_at) \
                     VALUES (?1, ?2, ?3)",
                    rusqlite::params![migration.version, migration.name, now],
                )
                .map_err(|e| SqliteError::Migration {
                    version: migration.version,
                    error: e.to_string(),
                })?;
                applied_version = migration.version;
                continue;
            }
        }

        let tx = conn.transaction().map_err(|e| SqliteError::Migration {
            version: migration.version,
            error: e.to_string(),
        })?;

        let up_sql = if migration.version == 5 {
            build_v5_event_observability_sql(&tx).map_err(|e| SqliteError::Migration {
                version: migration.version,
                error: e.to_string(),
            })?
        } else {
            migration.up.to_string()
        };

        tx.execute_batch(&up_sql)
            .map_err(|e| SqliteError::Migration {
                version: migration.version,
                error: e.to_string(),
            })?;

        let now = chrono::Utc::now().timestamp_micros();
        tx.execute(
            "INSERT INTO _schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![migration.version, migration.name, now],
        )
        .map_err(|e| SqliteError::Migration {
            version: migration.version,
            error: e.to_string(),
        })?;

        tx.commit().map_err(|e| SqliteError::Migration {
            version: migration.version,
            error: e.to_string(),
        })?;

        applied_version = migration.version;
    }

    Ok(applied_version)
}

fn table_has_column(
    conn: &Connection,
    table: &'static str,
    column: &'static str,
) -> Result<bool, rusqlite::Error> {
    conn.query_row(
        "SELECT COUNT(*) > 0 FROM pragma_table_info(?1) WHERE name = ?2",
        rusqlite::params![table, column],
        |row| row.get(0),
    )
}

fn build_v5_event_observability_sql(conn: &Connection) -> Result<String, rusqlite::Error> {
    let mut sql = String::new();
    for (column, ddl) in [
        (
            "kind",
            "ALTER TABLE events ADD COLUMN kind TEXT NOT NULL DEFAULT 'audit';",
        ),
        (
            "payload",
            "ALTER TABLE events ADD COLUMN payload TEXT NOT NULL DEFAULT '{}';",
        ),
        (
            "payload_schema_version",
            "ALTER TABLE events ADD COLUMN payload_schema_version INTEGER NOT NULL DEFAULT 1;",
        ),
        (
            "profile_state_version",
            "ALTER TABLE events ADD COLUMN profile_state_version INTEGER;",
        ),
        (
            "session_id",
            "ALTER TABLE events ADD COLUMN session_id TEXT;",
        ),
        (
            "aggregate_kind",
            "ALTER TABLE events ADD COLUMN aggregate_kind TEXT;",
        ),
        (
            "aggregate_id",
            "ALTER TABLE events ADD COLUMN aggregate_id TEXT;",
        ),
    ] {
        if !table_has_column(conn, "events", column)? {
            sql.push_str(ddl);
        }
    }
    // Migrate legacy data column into payload if both exist.
    if table_has_column(conn, "events", "data")? && table_has_column(conn, "events", "payload")? {
        sql.push_str("UPDATE events SET payload = data WHERE data IS NOT NULL AND data <> '';");
    }
    sql.push_str(
        "CREATE TABLE IF NOT EXISTS event_observations (\
            event_id TEXT NOT NULL,\
            entity_id TEXT NOT NULL,\
            referent_kind TEXT NOT NULL,\
            role TEXT NOT NULL,\
            position INTEGER NOT NULL,\
            PRIMARY KEY (event_id, role, position)\
        );\
        CREATE INDEX IF NOT EXISTS idx_events_kind ON events(kind);\
        CREATE INDEX IF NOT EXISTS idx_events_session ON events(namespace, session_id, created_at, id);\
        CREATE INDEX IF NOT EXISTS idx_events_ns_created_id ON events(namespace, created_at DESC, id DESC);\
        CREATE INDEX IF NOT EXISTS idx_events_payload_proposal_id ON events(json_extract(payload, '$.proposal_id'));\
        CREATE INDEX IF NOT EXISTS idx_event_obs_entity ON event_observations(entity_id, role);\
        CREATE INDEX IF NOT EXISTS idx_event_obs_event_role ON event_observations(event_id, role);",
    );
    Ok(sql)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn open_memory() -> Connection {
        Connection::open_in_memory().expect("in-memory connection")
    }

    #[test]
    fn fresh_db_migrates_to_latest() {
        let mut conn = open_memory();
        let version = run_migrations(&mut conn).expect("migrations should succeed");
        assert_eq!(version, 5);

        // Verify the tracking table has rows for V1 through V5.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version IN (1, 2, 3, 4, 5)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 5);

        // Verify the entities table was created.
        let tbl_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='entities'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tbl_count, 1);

        // Verify V2 added the name column to notes.
        let col_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('notes') WHERE name = 'name'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(col_count, 1, "V2 must add name column to notes");

        // Verify V5 added event observability columns to events.
        for col in [
            "kind",
            "payload",
            "payload_schema_version",
            "profile_state_version",
            "session_id",
            "aggregate_kind",
            "aggregate_id",
        ] {
            let exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM pragma_table_info('events') WHERE name = ?1",
                    [col],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(exists, "V5 must add events.{col}");
        }

        // Verify event_observations table exists.
        let obs_tbl: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='event_observations'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(obs_tbl, 1, "V5 must create event_observations table");

        // Verify V5 indexes exist.
        for idx in [
            "idx_events_ns_created_id",
            "idx_events_session",
            "idx_events_payload_proposal_id",
            "idx_event_obs_entity",
            "idx_event_obs_event_role",
        ] {
            let exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='index' AND name=?1",
                    [idx],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(exists, "V5 must create index {idx}");
        }
    }

    #[test]
    fn run_migrations_twice_is_idempotent() {
        let mut conn = open_memory();
        let v1 = run_migrations(&mut conn).expect("first run");
        let v2 = run_migrations(&mut conn).expect("second run");
        assert_eq!(v1, 5);
        assert_eq!(v2, 5);

        // Should still have exactly five rows in the tracking table (V1–V5).
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM _schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 5);
    }

    #[test]
    fn failed_migration_rolls_back() {
        let bad_v6 = VersionedMigration {
            version: 6,
            name: "bad_migration",
            up: "THIS IS NOT VALID SQL;",
        };

        let mut conn = open_memory();

        // Apply all real migrations (V1–V5) so the DB is at V5.
        run_migrations(&mut conn).expect("V1–V5 should apply cleanly");

        // Now manually drive the bad V6 migration to check rollback behaviour.
        let result = apply_single_migration(&mut conn, &bad_v6);
        assert!(result.is_err(), "bad migration should return error");

        // DB should still be at V5 — no V6 row in tracking.
        let v6_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version = 6",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(v6_count, 0, "V6 must not be recorded after rollback");

        // V1 through V5 should still be there.
        let applied_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version IN (1, 2, 3, 4, 5)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(applied_count, 5, "V1 through V5 must still be recorded");
    }

    #[test]
    fn store_ddl_then_migrations_is_idempotent() {
        use crate::stores::note::ensure_notes_schema;

        let mut conn = open_memory();

        // Simulate the StorageBackend path: store DDL creates notes table
        // WITH the name column (NOTES_DDL includes it for test convenience).
        ensure_notes_schema(&conn).expect("store DDL should create notes");

        // Verify name column exists from DDL.
        let has_name: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM pragma_table_info('notes') WHERE name = 'name'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(has_name, "NOTES_DDL should include name column");

        // Now run versioned migrations — V2 should detect the existing column
        // and skip the ALTER TABLE without error. V5 adds event observability schema.
        let version = run_migrations(&mut conn).expect("migrations after store DDL");
        assert_eq!(version, 5);

        // V2 should be recorded as applied (skipped but tracked).
        let v2_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version = 2",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            v2_count, 1,
            "V2 must be recorded even when column pre-exists"
        );
    }

    #[test]
    fn store_ddl_then_event_migration_is_idempotent() {
        use crate::stores::event::ensure_events_schema;

        let mut conn = open_memory();

        // Simulate the StorageBackend path: ensure_events_schema creates the
        // events table WITH the new columns. Running V5 on top must not fail.
        ensure_events_schema(&conn).expect("store DDL should create events");

        let version = run_migrations(&mut conn).expect("migrations after events store DDL");
        assert_eq!(version, 5, "must reach V5 even when events DDL ran first");

        let v5_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version = 5",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v5_count, 1, "V5 must be recorded");
    }

    /// Helper: apply a single migration in a transaction, recording it in the
    /// tracking table. Extracted here for use in the rollback test only.
    fn apply_single_migration(
        conn: &mut Connection,
        migration: &VersionedMigration,
    ) -> Result<(), SqliteError> {
        let tx = conn.transaction().map_err(|e| SqliteError::Migration {
            version: migration.version,
            error: e.to_string(),
        })?;

        tx.execute_batch(migration.up)
            .map_err(|e| SqliteError::Migration {
                version: migration.version,
                error: e.to_string(),
            })?;

        let now = chrono::Utc::now().timestamp_micros();
        tx.execute(
            "INSERT INTO _schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![migration.version, migration.name, now],
        )
        .map_err(|e| SqliteError::Migration {
            version: migration.version,
            error: e.to_string(),
        })?;

        tx.commit().map_err(|e| SqliteError::Migration {
            version: migration.version,
            error: e.to_string(),
        })?;

        Ok(())
    }
}

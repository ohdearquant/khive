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
        kind TEXT NOT NULL,\
        actor_id TEXT,\
        agent_id TEXT,\
        query_text TEXT,\
        outcome TEXT NOT NULL,\
        payload TEXT NOT NULL DEFAULT '{}',\
        created_at INTEGER NOT NULL\
    );\
    CREATE INDEX IF NOT EXISTS idx_events_namespace ON events(namespace);\
    CREATE INDEX IF NOT EXISTS idx_events_kind ON events(kind);\
    CREATE INDEX IF NOT EXISTS idx_events_created ON events(created_at DESC);\
";

/// All known migrations, ordered by ascending version.
///
/// To add a new migration: append a `VersionedMigration` entry with
/// `version = <last_version + 1>`. The version sequence must be contiguous
/// (1, 2, 3, ...); `run_migrations` returns an error on gaps.
pub const MIGRATIONS: &[VersionedMigration] = &[VersionedMigration {
    version: 1,
    name: "initial_schema",
    up: V1_UP,
}];

const MIGRATION_TRACKING_TABLE: &str = "\
    CREATE TABLE IF NOT EXISTS _schema_migrations (\
        version   INTEGER PRIMARY KEY,\
        name      TEXT NOT NULL,\
        applied_at INTEGER NOT NULL\
    );\
";

/// Detect whether this DB was created before `_schema_migrations` was introduced
/// but already has the V1 schema applied (the `entities` table exists).
fn is_legacy_v1_db(conn: &Connection) -> bool {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='entities'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    count > 0
}

/// Apply all unapplied migrations from `MIGRATIONS` in order.
///
/// Returns the highest version now applied, or `0` if the DB is empty and no
/// migrations exist.
///
/// # Idempotency
///
/// Safe to call multiple times. Already-applied migrations are skipped.
///
/// # Existing DBs
///
/// If `_schema_migrations` is absent but the `entities` table exists (indicating
/// a DB created before ADR-022), V1 is recorded as already applied without
/// re-running the DDL. This preserves existing data.
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
    // Validate that the MIGRATIONS array is contiguous.
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

    // Check whether _schema_migrations table already exists.
    let tracking_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='_schema_migrations'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);

    if !tracking_exists {
        // Create the tracking table.
        conn.execute_batch(MIGRATION_TRACKING_TABLE)?;

        // If this looks like a pre-ADR-022 DB, seed V1 as applied.
        if is_legacy_v1_db(conn) {
            let now = chrono::Utc::now().timestamp_micros();
            conn.execute(
                "INSERT OR IGNORE INTO _schema_migrations (version, name, applied_at) \
                 VALUES (?1, ?2, ?3)",
                rusqlite::params![1u32, "initial_schema", now],
            )?;
        }
    }

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

        applied_version = migration.version;
    }

    Ok(applied_version)
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
    fn fresh_db_migrates_to_v1() {
        let mut conn = open_memory();
        let version = run_migrations(&mut conn).expect("migrations should succeed");
        assert_eq!(version, 1);

        // Verify the tracking table has a row for V1.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        // Verify the entities table was created.
        let tbl_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='entities'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tbl_count, 1);
    }

    #[test]
    fn run_migrations_twice_is_idempotent() {
        let mut conn = open_memory();
        let v1 = run_migrations(&mut conn).expect("first run");
        let v2 = run_migrations(&mut conn).expect("second run");
        assert_eq!(v1, 1);
        assert_eq!(v2, 1);

        // Should still have exactly one row in the tracking table.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM _schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn failed_migration_rolls_back() {
        // Build a minimal test-only migration array with a bad V99.
        let bad_migrations: &[VersionedMigration] = &[
            VersionedMigration {
                version: 1,
                name: "initial_schema",
                up: V1_UP,
            },
            VersionedMigration {
                version: 2,
                name: "bad_migration",
                up: "THIS IS NOT VALID SQL;",
            },
        ];

        let mut conn = open_memory();

        // Apply V1 manually via the real run_migrations so the DB is at V1.
        run_migrations(&mut conn).expect("V1 should apply cleanly");

        // Now manually drive the bad V2 migration to check rollback behaviour.
        let result = apply_single_migration(&mut conn, &bad_migrations[1]);
        assert!(result.is_err(), "bad migration should return error");

        // DB should still be at V1 — no V2 row in tracking.
        let v2_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version = 2",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(v2_count, 0, "V2 must not be recorded after rollback");

        // V1 should still be there.
        let v1_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(v1_count, 1, "V1 must still be recorded");
    }

    #[test]
    fn existing_db_without_tracking_table_is_seeded_as_v1() {
        let mut conn = open_memory();

        // Simulate a pre-ADR-022 DB: create the entities table directly
        // without _schema_migrations.
        conn.execute_batch(
            "CREATE TABLE entities (id TEXT PRIMARY KEY, namespace TEXT NOT NULL, \
             kind TEXT NOT NULL, name TEXT NOT NULL, description TEXT, \
             properties TEXT, tags TEXT NOT NULL DEFAULT '[]', \
             created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, \
             deleted_at INTEGER);",
        )
        .expect("setup entities table");

        // run_migrations should detect the existing table and seed V1.
        let version = run_migrations(&mut conn).expect("should seed existing DB as V1");
        assert_eq!(version, 1);

        let v1_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(v1_count, 1);
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

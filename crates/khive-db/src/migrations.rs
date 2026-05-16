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
    fn fresh_db_migrates_to_latest() {
        let mut conn = open_memory();
        let version = run_migrations(&mut conn).expect("migrations should succeed");
        assert_eq!(version, 2);

        // Verify the tracking table has rows for V1 and V2.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version IN (1, 2)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);

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
    }

    #[test]
    fn run_migrations_twice_is_idempotent() {
        let mut conn = open_memory();
        let v1 = run_migrations(&mut conn).expect("first run");
        let v2 = run_migrations(&mut conn).expect("second run");
        assert_eq!(v1, 2);
        assert_eq!(v2, 2);

        // Should still have exactly two rows in the tracking table (V1 + V2).
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM _schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn failed_migration_rolls_back() {
        let bad_v3 = VersionedMigration {
            version: 3,
            name: "bad_migration",
            up: "THIS IS NOT VALID SQL;",
        };

        let mut conn = open_memory();

        // Apply all real migrations (V1 + V2) so the DB is at V2.
        run_migrations(&mut conn).expect("V1+V2 should apply cleanly");

        // Now manually drive the bad V3 migration to check rollback behaviour.
        let result = apply_single_migration(&mut conn, &bad_v3);
        assert!(result.is_err(), "bad migration should return error");

        // DB should still be at V2 — no V3 row in tracking.
        let v3_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version = 3",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(v3_count, 0, "V3 must not be recorded after rollback");

        // V1 and V2 should still be there.
        let applied_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version IN (1, 2)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(applied_count, 2, "V1 and V2 must still be recorded");
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

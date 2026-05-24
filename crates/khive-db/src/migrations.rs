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

/// V5: Add `status` column to notes; make `salience` and `decay_factor` nullable.
///
/// SQLite does not support `ALTER COLUMN` to change NOT NULL constraints, so the
/// salience/decay_factor nullability change is handled by rewriting the column
/// defaults: the columns already exist (added in V1) and will accept NULL when
/// inserted without a value. The `NOT NULL DEFAULT` constraint in V1 means any
/// existing rows already have a value; to allow NULLs going forward, SQLite
/// requires a full table rebuild — but since all existing values are valid f64,
/// we leave the constraint in place for existing rows and rely on application-
/// level logic (`NOTES_DDL` in stores/note.rs) to use nullable columns for new
/// tables. For production databases that went through V1, the application layer
/// handles NULLs via `Option<f64>` and the `NOT NULL DEFAULT` remains harmless
/// (inserts from the application always set these columns or leave them NULL via
/// the new nullable DDL path). The only structural change this migration makes
/// is adding the `status` column with a sensible default.
const V5_NOTE_STATUS_AND_NULLABLE_METRICS: &str = "\
    ALTER TABLE notes ADD COLUMN status TEXT NOT NULL DEFAULT 'active';\
";

/// V6: Add merge tombstone columns to entities.
///
/// `merged_into` stores the UUID of the entity this one was merged into.
/// `merge_event_id` is an opaque event ID for auditing. Both are nullable;
/// non-NULL only when the entity has been tombstoned by a merge.
/// The index on (namespace, merged_into) allows efficient lookup of all
/// entities that were merged into a given target.
///
/// ENTITIES_DDL in stores/entity.rs already includes these columns for new
/// databases (created via `CREATE TABLE IF NOT EXISTS`). The migration handles
/// the upgrade path for existing production databases.
const V6_ENTITY_TOMBSTONE_COLUMNS: &str = "\
    ALTER TABLE entities ADD COLUMN merged_into TEXT;\
    ALTER TABLE entities ADD COLUMN merge_event_id TEXT;\
    CREATE INDEX IF NOT EXISTS idx_entities_merged_into ON entities(namespace, merged_into);\
";

/// V7: Make `salience` and `decay_factor` nullable in the notes table.
///
/// V1 created notes with `salience REAL NOT NULL DEFAULT 0.5` and
/// `decay_factor REAL NOT NULL DEFAULT 0.0`. SQLite does not support
/// `ALTER COLUMN` to remove a NOT NULL constraint, so a full table rebuild
/// is required. This migration rebuilds notes with the canonical nullable
/// schema that `NOTES_DDL` in stores/note.rs uses for fresh databases.
///
/// On databases bootstrapped via `NOTES_DDL` (all test paths and new
/// installs), salience/decay_factor are already nullable — the V7 idempotency
/// check detects this and skips the rebuild, recording V7 as applied.
const V7_NULLABLE_NOTE_METRICS: &str = "\
    CREATE TABLE notes_new (\
        id TEXT PRIMARY KEY,\
        namespace TEXT NOT NULL,\
        kind TEXT NOT NULL,\
        status TEXT NOT NULL DEFAULT 'active',\
        name TEXT,\
        content TEXT NOT NULL DEFAULT '',\
        salience REAL,\
        decay_factor REAL,\
        expires_at INTEGER,\
        properties TEXT,\
        created_at INTEGER NOT NULL,\
        updated_at INTEGER NOT NULL,\
        deleted_at INTEGER\
    );\
    INSERT INTO notes_new \
        (id, namespace, kind, status, name, content, salience, decay_factor, \
         expires_at, properties, created_at, updated_at, deleted_at) \
    SELECT \
        id, namespace, kind, status, name, content, salience, decay_factor, \
        expires_at, properties, created_at, updated_at, deleted_at \
    FROM notes;\
    DROP TABLE notes;\
    ALTER TABLE notes_new RENAME TO notes;\
    CREATE INDEX IF NOT EXISTS idx_notes_namespace ON notes(namespace);\
    CREATE INDEX IF NOT EXISTS idx_notes_kind ON notes(namespace, kind);\
    CREATE INDEX IF NOT EXISTS idx_notes_created ON notes(created_at DESC);\
";

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
        name: "note_status_and_nullable_metrics",
        up: V5_NOTE_STATUS_AND_NULLABLE_METRICS,
    },
    VersionedMigration {
        version: 6,
        name: "entity_tombstone_columns",
        up: V6_ENTITY_TOMBSTONE_COLUMNS,
    },
    VersionedMigration {
        version: 7,
        name: "nullable_note_metrics",
        up: V7_NULLABLE_NOTE_METRICS,
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

        // V5 adds `status` to notes.  NOTES_DDL in stores/note.rs already includes
        // `status`, so when a fresh schema is created via the store path (e.g. in
        // tests or StorageBackend::notes()), the column exists before V5 runs.
        // Detect and skip idempotently, recording the migration as applied.
        if migration.version == 5 {
            let col_exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM pragma_table_info('notes') WHERE name = 'status'",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            if col_exists {
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

        // V6 adds `merged_into` and `merge_event_id` to entities. ENTITIES_DDL in
        // stores/entity.rs already includes these columns for databases created via
        // the store path (e.g. in tests or StorageBackend::entities()). Detect and
        // skip idempotently, recording the migration as applied.
        if migration.version == 6 {
            let col_exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM pragma_table_info('entities') WHERE name = 'merged_into'",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            if col_exists {
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

        // V7 rebuilds the notes table to make salience/decay_factor nullable.
        // NOTES_DDL in stores/note.rs already declares them nullable for databases
        // created via the store path. If salience is already nullable (notnull=0),
        // skip the rebuild and record V7 as applied.
        if migration.version == 7 {
            let already_nullable: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM pragma_table_info('notes') \
                     WHERE name = 'salience' AND \"notnull\" = 0",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(false);
            if already_nullable {
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
        assert_eq!(version, 7);

        // Verify the tracking table has rows for V1 through V7.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version IN (1, 2, 3, 4, 5, 6, 7)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 7);

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

        // Verify V5 added the status column to notes.
        let status_col: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('notes') WHERE name = 'status'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status_col, 1, "V5 must add status column to notes");

        // Verify V6 added merged_into column to entities.
        let merged_into_col: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('entities') WHERE name = 'merged_into'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            merged_into_col, 1,
            "V6 must add merged_into column to entities"
        );

        // Verify V7 made salience nullable (notnull=0).
        let salience_notnull: i64 = conn
            .query_row(
                "SELECT \"notnull\" FROM pragma_table_info('notes') WHERE name = 'salience'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(salience_notnull, 0, "V7 must make salience nullable");
    }

    #[test]
    fn run_migrations_twice_is_idempotent() {
        let mut conn = open_memory();
        let v1 = run_migrations(&mut conn).expect("first run");
        let v2 = run_migrations(&mut conn).expect("second run");
        assert_eq!(v1, 7);
        assert_eq!(v2, 7);

        // Should still have exactly seven rows in the tracking table (V1 through V7).
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM _schema_migrations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 7);
    }

    #[test]
    fn failed_migration_rolls_back() {
        let bad_v8 = VersionedMigration {
            version: 8,
            name: "bad_migration",
            up: "THIS IS NOT VALID SQL;",
        };

        let mut conn = open_memory();

        // Apply all real migrations (V1 through V7) so the DB is at V7.
        run_migrations(&mut conn).expect("V1-V7 should apply cleanly");

        // Now manually drive the bad V8 migration to check rollback behaviour.
        let result = apply_single_migration(&mut conn, &bad_v8);
        assert!(result.is_err(), "bad migration should return error");

        // DB should still be at V7 — no V8 row in tracking.
        let v8_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version = 8",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(v8_count, 0, "V8 must not be recorded after rollback");

        // V1 through V7 should still be there.
        let applied_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version IN (1, 2, 3, 4, 5, 6, 7)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(applied_count, 7, "V1 through V7 must still be recorded");
    }

    #[test]
    fn store_ddl_then_migrations_is_idempotent() {
        use crate::stores::entity::ensure_entities_schema;
        use crate::stores::note::ensure_notes_schema;

        let mut conn = open_memory();

        // Simulate the StorageBackend path: store DDL creates notes table
        // WITH the name column (NOTES_DDL includes it for test convenience).
        ensure_notes_schema(&conn).expect("store DDL should create notes");

        // Simulate entity DDL creation (includes merged_into, merge_event_id).
        ensure_entities_schema(&conn).expect("store DDL should create entities");

        // Verify name column exists from DDL.
        let has_name: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM pragma_table_info('notes') WHERE name = 'name'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(has_name, "NOTES_DDL should include name column");

        // Now run versioned migrations — V2 should detect the existing name column
        // and skip; V5 should detect the existing status column and skip; V6 should
        // detect the existing merged_into column and skip; V7 should detect that
        // salience is already nullable and skip; V4 adds the unique triple index.
        let version = run_migrations(&mut conn).expect("migrations after store DDL");
        assert_eq!(version, 7);

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

        // V5 should be recorded as applied (skipped but tracked).
        let v5_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version = 5",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            v5_count, 1,
            "V5 must be recorded even when status column pre-exists via NOTES_DDL"
        );

        // V6 should be recorded as applied (skipped but tracked).
        let v6_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version = 6",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            v6_count, 1,
            "V6 must be recorded even when merged_into column pre-exists via ENTITIES_DDL"
        );

        // V7 should be recorded as applied (skipped but tracked — NOTES_DDL already
        // creates salience as nullable).
        let v7_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _schema_migrations WHERE version = 7",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            v7_count, 1,
            "V7 must be recorded even when salience is already nullable via NOTES_DDL"
        );
    }

    /// Verify that V7 rebuilds a V1-era notes table so salience/decay_factor
    /// accept NULL, unblocking `create_note` with `salience=None` on migrated DBs.
    #[test]
    fn v1_to_v7_allows_null_salience() {
        let mut conn = open_memory();

        // Bootstrap the schema tracking table and create the full V1-era schema.
        // The notes table uses NOT NULL DEFAULT on salience/decay_factor as V1 did.
        conn.execute_batch(MIGRATION_TRACKING_TABLE).unwrap();
        conn.execute_batch(
            "CREATE TABLE entities (\
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
            CREATE TABLE graph_edges (\
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
            CREATE TABLE notes (\
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
            CREATE TABLE events (\
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
            );",
        )
        .unwrap();

        // Record V1 as already applied so run_migrations starts at V2.
        let now = chrono::Utc::now().timestamp_micros();
        conn.execute(
            "INSERT INTO _schema_migrations (version, name, applied_at) VALUES (1, 'initial_schema', ?1)",
            rusqlite::params![now],
        )
        .unwrap();

        // Run V2-V7 migrations.
        let version = run_migrations(&mut conn).expect("migrations should succeed");
        assert_eq!(version, 7);

        // After V7, salience must be nullable (notnull=0).
        let notnull: i64 = conn
            .query_row(
                "SELECT \"notnull\" FROM pragma_table_info('notes') WHERE name = 'salience'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(notnull, 0, "salience must be nullable after V7");

        // Inserting a note without salience must succeed.
        conn.execute(
            "INSERT INTO notes (id, namespace, kind, status, content, created_at, updated_at) \
             VALUES ('test-id', 'ns', 'observation', 'active', '', 1, 1)",
            [],
        )
        .expect("inserting note with NULL salience must succeed after V7");

        let stored_salience: Option<f64> = conn
            .query_row(
                "SELECT salience FROM notes WHERE id = 'test-id'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            stored_salience.is_none(),
            "salience must be NULL when not supplied"
        );
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

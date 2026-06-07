//! Schema migration system for the SQLite storage layer.
//!
//! Two APIs coexist:
//! - **Legacy per-service migrations** (`ServiceSchemaPlan` / `apply_schema_plan`):
//!   used by pack-scoped schemas.
//! - **Versioned migrations** (`MIGRATIONS` / `run_migrations`): the forward-only
//!   migration pipeline for the core tables.

use rusqlite::Connection;

use crate::error::SqliteError;

// =============================================================================
// Legacy per-service migration API (preserved for backward compatibility)
// =============================================================================

/// A single legacy migration step within a `ServiceSchemaPlan`.
pub struct Migration {
    /// Unique identifier for this migration.
    pub id: &'static str,
    /// SQL to apply (forward direction).
    pub up_sql: &'static str,
    /// SQL to revert (optional).
    pub down_sql: Option<&'static str>,
    /// Optional predicate: returns true if migration was already applied
    /// through a mechanism other than the migration tracker.
    pub is_already_applied: Option<fn(&Connection) -> bool>,
}

/// A pack-scoped schema plan containing migrations for SQLite and Postgres.
pub struct ServiceSchemaPlan {
    /// Service name used as a key in the `_schema_versions` tracking table.
    pub service: &'static str,
    /// SQLite-specific migration steps, applied in order.
    pub sqlite: &'static [Migration],
    /// Postgres-specific migration steps (reserved for future use).
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

/// Apply a pack-scoped schema plan, tracking each migration in `_schema_versions`.
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
// Versioned migration system
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

// V1: complete schema, loaded from sql/schema.sql.
// Fresh-start repo (v0.2.8) — all schema in one migration, no incremental versions.
const V1_UP: &str = include_str!("../sql/schema.sql");

/// DDL for the `_embedding_models` registry table.
///
/// Shared between the V1 schema and the belt-and-suspenders creation in
/// `StorageBackend::vectors_for_namespace`. Both sites reference this constant so
/// the schema cannot silently diverge if the registry evolves.
pub const EMBEDDING_MODELS_DDL: &str = "\
    CREATE TABLE IF NOT EXISTS _embedding_models (\
        id              BLOB PRIMARY KEY,\
        engine_name     TEXT NOT NULL,\
        model_id        TEXT NOT NULL,\
        key_version     TEXT NOT NULL,\
        dim             INTEGER NOT NULL,\
        output_dim      INTEGER,\
        status          TEXT NOT NULL CHECK (status IN ('pending', 'active', 'superseded', 'archived')),\
        activated_at    INTEGER,\
        superseded_at   INTEGER,\
        superseded_by   BLOB,\
        canonical_key   BLOB NOT NULL UNIQUE,\
        created_at      INTEGER NOT NULL\
    );\
    CREATE UNIQUE INDEX IF NOT EXISTS idx_embed_models_one_active \
        ON _embedding_models(engine_name) WHERE status = 'active';\
    CREATE INDEX IF NOT EXISTS idx_embed_models_engine_status \
        ON _embedding_models(engine_name, status);";

/// All versioned migrations in ascending order, applied by `run_migrations`.
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

/// Apply all unapplied migrations in order. Idempotent; each migration runs in its own transaction.
/// Errors on non-contiguous version array or failed migration.
/// Read the applied schema version from an open connection **without** running
/// migrations. Returns 0 when the `_schema_migrations` ledger is absent (an
/// un-migrated or empty database). Never writes.
pub fn read_schema_version(conn: &Connection) -> u32 {
    conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM _schema_migrations",
        [],
        |row| row.get(0),
    )
    .unwrap_or(0)
}

/// Open `path` read-only and report its applied schema version without creating
/// or migrating the file. The caller must ensure `path` exists — opening a
/// missing file read-only errors rather than creating it. This is the path used
/// by schema-inspection commands that must not mutate the database.
pub fn inspect_schema_version(path: &std::path::Path) -> Result<u32, SqliteError> {
    let conn = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    Ok(read_schema_version(&conn))
}

pub fn run_migrations(conn: &mut Connection) -> Result<u32, SqliteError> {
    conn.execute_batch(MIGRATION_TRACKING_TABLE)?;

    let current_version: u32 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM _schema_migrations",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    // A database whose recorded version is ahead of the latest known migration
    // predates the consolidated V1 baseline (ADR-015) — e.g. it still carries the
    // pre-consolidation V2..V22 ledger — or was written by a newer build. Either
    // way the baseline schema would be silently skipped, leaving the process on a
    // stale schema. Fail loudly instead of corrupting silently.
    let latest_version = MIGRATIONS.last().map(|m| m.version).unwrap_or(0);
    if current_version > latest_version {
        return Err(SqliteError::InvalidData(format!(
            "database schema version {current_version} is ahead of the latest known migration \
             {latest_version}. This database predates the consolidated baseline (ADR-015) or was \
             written by a newer build. Recreate it from the current schema; in-place downgrade is \
             not supported."
        )));
    }

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

pub struct EmbeddingModelRegistryRecord {
    /// Vector engine name (e.g. `"paraphrase"`).
    pub engine_name: String,
    /// Model identifier (e.g. `"all-minilm-l6-v2"`).
    pub model_id: String,
    /// Canonical deduplication key combining engine and model.
    pub key_version: String,
    /// Embedding dimensionality.
    pub dimensions: u32,
    /// Lifecycle status (`"active"` or `"superseded"`).
    pub status: String,
    /// Epoch timestamp when the model was activated.
    pub activated_at: Option<i64>,
    /// Epoch timestamp when the model was superseded.
    pub superseded_at: Option<i64>,
}

/// Query the `_embedding_models` registry.
///
/// Opens the database at `db` (defaults to `~/.khive/khive.db`) and
/// returns all registry rows, optionally filtered by `engine_name`.
/// Returns an empty vec if the database or table does not exist.
pub fn query_embedding_models(
    db: Option<&std::path::Path>,
    engine_filter: Option<&str>,
) -> Result<Vec<EmbeddingModelRegistryRecord>, SqliteError> {
    let path = db.map(std::path::Path::to_path_buf).unwrap_or_else(|| {
        std::env::var("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join(".khive/khive.db")
    });
    if !path.exists() {
        return Ok(Vec::new());
    }

    let conn = Connection::open(path)?;
    let exists: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM sqlite_master \
         WHERE type='table' AND name='_embedding_models'",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(Vec::new());
    }

    let sql = if engine_filter.is_some() {
        "SELECT engine_name, model_id, key_version, dim, status, activated_at, superseded_at \
         FROM _embedding_models WHERE engine_name = ?1 \
         ORDER BY engine_name, activated_at IS NULL, activated_at"
    } else {
        "SELECT engine_name, model_id, key_version, dim, status, activated_at, superseded_at \
         FROM _embedding_models \
         ORDER BY engine_name, activated_at IS NULL, activated_at"
    };
    let mut stmt = conn.prepare(sql)?;
    let map_row = |row: &rusqlite::Row<'_>| {
        Ok(EmbeddingModelRegistryRecord {
            engine_name: row.get(0)?,
            model_id: row.get(1)?,
            key_version: row.get(2)?,
            dimensions: row.get::<_, i64>(3)? as u32,
            status: row.get(4)?,
            activated_at: row.get(5)?,
            superseded_at: row.get(6)?,
        })
    };

    if let Some(engine) = engine_filter {
        stmt.query_map([engine], map_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    } else {
        stmt.query_map([], map_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
#[path = "migrations_tests.rs"]
mod tests;

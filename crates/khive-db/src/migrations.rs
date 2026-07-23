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

const SCHEMA_VERSION_TABLE: &str = include_str!("../sql/schema-version-table.sql");

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

        let tx =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        tx.execute_batch(migration.up_sql)?;

        tx.execute(
            "INSERT INTO _schema_versions (service, migration_id, applied_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![
                plan.service,
                migration.id,
                chrono::Utc::now().timestamp_micros(),
            ],
        )?;
        tx.commit()?;
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

const V2_UP: &str = include_str!("../sql/002-narrow-fts-sections-update-trigger.sql");

const V3_UP: &str = include_str!("../sql/003-backfill-domain-mirror-atoms.sql");

const V4_UP: &str = include_str!("../sql/004-fts-consolidation.sql");

const V5_UP: &str = include_str!("../sql/005-unique-comm-external-id.sql");

const V6_UP: &str = include_str!("../sql/006-brain-retune-driver.sql");

const V7_UP: &str = include_str!("../sql/007-notes-seq.sql");

const V8_UP: &str = include_str!("../sql/008-notes-seq-repair.sql");

const V9_UP: &str = include_str!("../sql/009-entities-name-ci-index.sql");

const V10_UP: &str = include_str!("../sql/010-entities-content-ref.sql");

const V11_UP: &str = include_str!("../sql/011-ann-write-log.sql");

const V12_UP: &str = include_str!("../sql/012-ann-write-log-model-seq-index.sql");

const V13_UP: &str = include_str!("../sql/013-concept-single-origin.sql");

/// DDL for the `ann_write_log` delta table.
///
/// Shared between migration V11 and the belt-and-suspenders creation in
/// `StorageBackend::vectors_for_namespace` (same pattern as
/// [`EMBEDDING_MODELS_DDL`]): every database that hosts `vec_*` tables must
/// also have the write log, or vector writes would fail on databases opened
/// without `run_migrations()`. The `.sql` file is `IF NOT EXISTS`-idempotent.
pub const ANN_WRITE_LOG_DDL: &str = V11_UP;

/// DDL for the `ann_write_log` model/kind/field-leading index (ADR-118 §"Cost
/// bound"), shared between migration V12 and the belt-and-suspenders creation
/// in `StorageBackend::vectors_for_namespace` for the same reason as
/// [`ANN_WRITE_LOG_DDL`].
pub const ANN_WRITE_LOG_MODEL_SEQ_INDEX_DDL: &str = V12_UP;

/// DDL for the `_embedding_models` registry table.
///
/// Shared between the V1 schema and the belt-and-suspenders creation in
/// `StorageBackend::vectors_for_namespace`. Both sites reference this constant so
/// the schema cannot silently diverge if the registry evolves.
pub const EMBEDDING_MODELS_DDL: &str = include_str!("../sql/embedding-models-ddl.sql");

/// All versioned migrations in ascending order, applied by `run_migrations`.
pub const MIGRATIONS: &[VersionedMigration] = &[
    VersionedMigration {
        version: 1,
        name: "initial_schema",
        up: V1_UP,
    },
    VersionedMigration {
        version: 2,
        name: "narrow_fts_sections_update_trigger",
        up: V2_UP,
    },
    VersionedMigration {
        version: 3,
        name: "backfill_domain_mirror_atoms",
        up: V3_UP,
    },
    VersionedMigration {
        version: 4,
        name: "fts_consolidation",
        up: V4_UP,
    },
    VersionedMigration {
        version: 5,
        name: "unique_comm_message_external_id",
        up: V5_UP,
    },
    VersionedMigration {
        version: 6,
        name: "brain_retune_driver",
        up: V6_UP,
    },
    VersionedMigration {
        version: 7,
        name: "notes_seq",
        up: V7_UP,
    },
    VersionedMigration {
        version: 8,
        name: "notes_seq_repair",
        up: V8_UP,
    },
    VersionedMigration {
        version: 9,
        name: "entities_name_ci_index",
        up: V9_UP,
    },
    VersionedMigration {
        version: 10,
        name: "entities_content_ref",
        up: V10_UP,
    },
    VersionedMigration {
        version: 11,
        name: "ann_write_log",
        up: V11_UP,
    },
    VersionedMigration {
        version: 12,
        name: "ann_write_log_model_seq_index",
        up: V12_UP,
    },
    VersionedMigration {
        version: 13,
        name: "concept_single_origin",
        up: V13_UP,
    },
];

const MIGRATION_TRACKING_TABLE: &str = include_str!("../sql/schema-migrations-table.sql");

/// Read-only schema metadata used by administrative diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchemaInspection {
    /// Highest version recorded in the migration ledger.
    pub version: u32,
    /// Whether the ledger belongs to the pre-consolidation migration lineage.
    pub pre_consolidation: bool,
}

fn is_pre_consolidation_ledger(conn: &Connection) -> Result<bool, SqliteError> {
    // V1 used the same name in both lineages; the historical V2 and V22 names
    // are unambiguous signals that this ledger predates consolidation.
    let has_legacy_name: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM _schema_migrations \
         WHERE (version = 2 AND name = ?1) OR (version = 22 AND name = ?2))",
        ["add_name_to_notes", "knowledge_lifecycle_status"],
        |row| row.get(0),
    )?;
    if has_legacy_name {
        return Ok(true);
    }

    let highest_version: u32 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM _schema_migrations",
        [],
        |row| row.get(0),
    )?;
    if highest_version != 1 {
        return Ok(false);
    }

    // Consolidated V1 includes the event-kind discriminator; historical V1
    // did not gain it until a later migration in the retired lineage.
    let has_consolidated_v1_shape: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('events') WHERE name = 'kind')",
        [],
        |row| row.get(0),
    )?;
    Ok(!has_consolidated_v1_shape)
}

fn validate_schema_compatibility(
    conn: &Connection,
    store_version: u32,
    max_known_migration: u32,
) -> Result<(), SqliteError> {
    if is_pre_consolidation_ledger(conn)? {
        return Err(SqliteError::InvalidData(format!(
            "database schema version {store_version} predates the consolidated baseline; \
             recreate it from the current schema because in-place upgrade is not supported."
        )));
    }
    if store_version > max_known_migration {
        return Err(SqliteError::SchemaTooNew {
            max_known_migration,
            store_version,
        });
    }
    Ok(())
}

/// Apply all unapplied migrations in order. Idempotent; each migration runs in its own transaction.
/// Errors on non-contiguous version array or failed migration.
/// Read the applied schema version from an open connection **without** running
/// migrations. Returns 0 when the `_schema_migrations` ledger is absent (an
/// un-migrated or empty database); any other failure (BUSY, IO) propagates —
/// collapsing it to 0 would misreport a live database as un-migrated. Never
/// writes.
pub fn read_schema_version(conn: &Connection) -> Result<u32, SqliteError> {
    match conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM _schema_migrations",
        [],
        |row| row.get(0),
    ) {
        Ok(version) => Ok(version),
        Err(rusqlite::Error::SqliteFailure(_, Some(ref msg)))
            if msg.contains("no such table: _schema_migrations") =>
        {
            Ok(0)
        }
        Err(e) => Err(e.into()),
    }
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
    read_schema_version(&conn)
}

/// Open `path` read-only and inspect both its applied version and migration lineage.
/// An absent migration ledger is reported as version 0 in the consolidated lineage.
pub fn inspect_schema(path: &std::path::Path) -> Result<SchemaInspection, SqliteError> {
    let conn = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let version = read_schema_version(&conn)?;
    let pre_consolidation = version > 0 && is_pre_consolidation_ledger(&conn)?;
    Ok(SchemaInspection {
        version,
        pre_consolidation,
    })
}

#[cfg(test)]
pub(crate) mod test_sync {
    use std::sync::atomic::AtomicU32;
    use std::sync::{Arc, Barrier, Mutex};

    /// When set, `run_migrations_locked` parks after its initial (stale)
    /// ledger read until every racing thread has arrived — forcing the
    /// contended interleaving the concurrent-boot test asserts on.
    pub(crate) static STALE_READ_BARRIER: Mutex<Option<Arc<Barrier>>> = Mutex::new(None);
    /// Counts entries into the under-lock sibling fast-forward branch.
    pub(crate) static LOCKED_FAST_FORWARDS: AtomicU32 = AtomicU32::new(0);
    /// Set by the SQLite busy handler installed on participating connections:
    /// `true` means SQLite itself reported a blocked lock acquisition to the
    /// loser — actual contention, not merely an intended attempt.
    pub(crate) static BUSY_OBSERVED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    /// Busy handler for participating test connections: records that SQLite
    /// observed a busy acquisition, then keeps retrying.
    pub(crate) fn record_busy(_count: i32) -> bool {
        BUSY_OBSERVED.store(true, std::sync::atomic::Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(1));
        true
    }

    /// Set by the winner immediately before committing its first migration
    /// transaction — i.e. before the write lock is first released.
    pub(crate) static WINNER_COMMITTED: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    /// Recorded by the loser when its first `BEGIN IMMEDIATE` returns: whether
    /// the winner had already committed at that moment. `true` is direct
    /// evidence the loser's lock acquisition blocked across the winner's held
    /// write lock rather than the two calls serializing by scheduler accident.
    pub(crate) static LOSER_SAW_WINNER_COMMIT: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    std::thread_local! {
        /// Opt-in flag: only threads that set this participate in the barrier,
        /// so unrelated tests migrating in parallel are never parked.
        pub(crate) static PARTICIPATE: std::cell::Cell<bool> =
            const { std::cell::Cell::new(false) };
        /// Whether this thread has already instrumented its first BEGIN.
        pub(crate) static FIRST_BEGIN_DONE: std::cell::Cell<bool> =
            const { std::cell::Cell::new(false) };
    }
}

pub fn run_migrations(conn: &mut Connection) -> Result<u32, SqliteError> {
    // Concurrent boots (multiple processes migrating the same file) contend on
    // the write lock below; a short hot-path busy_timeout cannot wait out a
    // sibling's migration. Raise-only to a 5s floor — never reduce a caller
    // whose configured timeout is already longer — and restore after.
    let prior_busy_ms: i64 = conn.query_row("PRAGMA busy_timeout", [], |row| row.get(0))?;
    let raised = prior_busy_ms < 5_000;
    if raised {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
    }
    let result = run_migrations_locked(conn);
    if raised {
        let _ = conn.busy_timeout(std::time::Duration::from_millis(prior_busy_ms.max(0) as u64));
    }
    result
}

fn run_migrations_locked(conn: &mut Connection) -> Result<u32, SqliteError> {
    conn.execute_batch(MIGRATION_TRACKING_TABLE)?;

    let current_version: u32 = read_schema_version(conn)?;

    // Deterministic-contention hook: parks every caller after the stale ledger
    // read (no lock held) until all racing test threads have observed it, so
    // they are then released to compete for the IMMEDIATE write lock below.
    #[cfg(test)]
    if test_sync::PARTICIPATE.with(|p| p.get()) {
        // Replaces the busy_timeout raised by `run_migrations` on this test
        // connection: records SQLite-observed contention, then keeps retrying.
        conn.busy_handler(Some(test_sync::record_busy))?;
        let barrier = test_sync::STALE_READ_BARRIER.lock().unwrap().clone();
        if let Some(barrier) = barrier {
            barrier.wait();
        }
    }

    let latest_version = MIGRATIONS.last().map(|m| m.version).unwrap_or(0);
    validate_schema_compatibility(conn, current_version, latest_version)?;

    let mut applied_version = current_version;
    // Floor advanced when a sibling's work is observed under the write lock,
    // so a losing process skips the remaining already-applied migrations
    // without opening a transaction for each.
    let mut skip_through = current_version;

    for migration in MIGRATIONS {
        if migration.version <= skip_through {
            applied_version = applied_version.max(migration.version);
            continue;
        }

        // IMMEDIATE: take the write lock up front so concurrent boots serialize
        // here instead of failing mid-migration when a DEFERRED transaction
        // upgrades to a write.
        #[cfg(test)]
        let instrumented_first_begin = test_sync::PARTICIPATE.with(|p| p.get())
            && !test_sync::FIRST_BEGIN_DONE.with(|f| f.get());
        #[cfg(test)]
        if instrumented_first_begin {
            test_sync::FIRST_BEGIN_DONE.with(|f| f.set(true));
        }
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| SqliteError::Migration {
                version: migration.version,
                error: e.to_string(),
            })?;

        // Re-check under the write lock: a sibling process may have applied
        // this migration (and possibly later ones) while we waited. Running
        // its DDL again would fail; fast-forward past everything it applied.
        let sibling_version: u32 = tx
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM _schema_migrations",
                [],
                |row| row.get(0),
            )
            .map_err(|e| SqliteError::Migration {
                version: migration.version,
                error: e.to_string(),
            })?;
        #[cfg(test)]
        if instrumented_first_begin {
            use std::sync::atomic::Ordering::SeqCst;
            if sibling_version == 0 {
                // Winner: hold the write lock until SQLite has reported a
                // busy acquisition to the loser (its busy handler fired) —
                // proof the loser's BEGIN is actually blocked on this held
                // lock, not merely intended. Bounded so a regression fails
                // the assertion instead of hanging the test.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while !test_sync::BUSY_OBSERVED.load(SeqCst) && std::time::Instant::now() < deadline
                {
                    std::thread::yield_now();
                }
            } else {
                // Loser: our first BEGIN just returned. Record whether the
                // winner had already committed — true means we blocked across
                // its held lock.
                test_sync::LOSER_SAW_WINNER_COMMIT
                    .store(test_sync::WINNER_COMMITTED.load(SeqCst), SeqCst);
            }
        }

        // The first guard ran before the write lock; re-check both lineage and
        // version in case another process advanced the ledger while we waited.
        validate_schema_compatibility(&tx, sibling_version, latest_version)?;

        if sibling_version >= migration.version {
            #[cfg(test)]
            test_sync::LOCKED_FAST_FORWARDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            skip_through = sibling_version.min(latest_version);
            applied_version = applied_version.max(migration.version);
            continue;
        }

        if migration.version == 13 {
            reject_preexisting_duplicate_concept_origins(&tx)?;
        }

        tx.execute_batch(migration.up)
            .map_err(|e| SqliteError::Migration {
                version: migration.version,
                error: e.to_string(),
            })?;

        let now = chrono::Utc::now().timestamp_micros();
        tx.execute(
            "INSERT INTO _schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(version) DO NOTHING",
            rusqlite::params![migration.version, migration.name, now],
        )
        .map_err(|e| SqliteError::Migration {
            version: migration.version,
            error: e.to_string(),
        })?;

        #[cfg(test)]
        if instrumented_first_begin {
            test_sync::WINNER_COMMITTED.store(true, std::sync::atomic::Ordering::SeqCst);
        }

        tx.commit().map_err(|e| SqliteError::Migration {
            version: migration.version,
            error: e.to_string(),
        })?;

        applied_version = migration.version;
    }

    Ok(applied_version)
}

/// Migration V13 (`concept_single_origin`) only installs enforcement triggers
/// going forward; it cannot retroactively decide which of two pre-existing
/// live origins for the same concept is correct. Auto-picking one would
/// silently discard a user's `introduced_by` edge, so a database that already
/// violates the invariant must fail the migration with the offending concept
/// ids named, rather than migrate into a state the new triggers cannot
/// express.
fn reject_preexisting_duplicate_concept_origins(conn: &Connection) -> Result<(), SqliteError> {
    let mut stmt = conn.prepare(
        "SELECT namespace, source_id, GROUP_CONCAT(DISTINCT target_id) AS origins \
         FROM graph_edges \
         WHERE relation = 'introduced_by' \
           AND deleted_at IS NULL \
           AND source_id IN (SELECT id FROM entities WHERE kind = 'concept' AND deleted_at IS NULL) \
         GROUP BY namespace, source_id \
         HAVING COUNT(DISTINCT target_id) > 1 \
         ORDER BY namespace, source_id",
    )?;
    let violations = stmt
        .query_map([], |row| {
            let namespace: String = row.get(0)?;
            let source_id: String = row.get(1)?;
            let origins: String = row.get(2)?;
            Ok(format!(
                "concept {source_id} in namespace {namespace} (origins: {origins})"
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    if violations.is_empty() {
        return Ok(());
    }

    Err(SqliteError::Migration {
        version: 13,
        error: format!(
            "cannot enforce the single-origin invariant: {} already have more than one live \
             introduced_by origin. Curate each listed concept down to a single live origin \
             (delete or supersede the extra introduced_by edges), then re-run migrations.",
            violations.join(", ")
        ),
    })
}

#[derive(Debug)]
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
    query_embedding_models_conn(&conn, engine_filter)
}

/// Query `_embedding_models` from an existing connection (testable without a file).
///
/// Returns an empty vec if the table does not exist.
pub(crate) fn query_embedding_models_conn(
    conn: &Connection,
    engine_filter: Option<&str>,
) -> Result<Vec<EmbeddingModelRegistryRecord>, SqliteError> {
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
        let dim_raw: i64 = row.get(3)?;
        let dimensions = u32::try_from(dim_raw).map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Integer,
                Box::new(std::io::Error::other(format!(
                    "_embedding_models.dim value {dim_raw} is outside the valid u32 range [0, {}]",
                    u32::MAX,
                ))),
            )
        })?;
        Ok(EmbeddingModelRegistryRecord {
            engine_name: row.get(0)?,
            model_id: row.get(1)?,
            key_version: row.get(2)?,
            dimensions,
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

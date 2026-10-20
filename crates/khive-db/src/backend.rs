//! Concrete storage backend providing capability traits.
//!
//! `StorageBackend` owns a `ConnectionPool` and provides factory methods for all
//! ten capability traits (`SqlAccess`, `NoteStore`, `EntityStore`, `GraphStore`,
//! `EventStore`, `VectorStore`, `SparseStore`, `TextSearch`, `BlobStore`, and
//! `AttachmentStore`). File-backed for production; in-memory for tests.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use rusqlite::OptionalExtension;

use crate::database_owner_identity::{DatabaseOwnerIdentity, DatabaseOwnerIdentityError};
use crate::error::SqliteError;
use crate::pool::{ConnectionPool, PoolConfig, WalCeilingPolicy};
use crate::stores::{agents, attachment, blob, entity, event, graph, note, sparse, text, vectors};

mod capability_factories;
mod code_map;
mod core_stores;
#[path = "backend/schema_readiness.rs"]
mod memory_visibility;
mod pack_schema;
mod policy_open;

#[cfg(test)]
#[path = "backend/filter_builder_tests.rs"]
mod filter_builder_tests;

mod namespace_listing;
pub use namespace_listing::NamespaceLiveness;

#[cfg(test)]
#[path = "backend/memory_visibility_tests.rs"]
mod memory_visibility_tests;

#[cfg(any(unix, windows))]
mod claimed_file_identity;

fn sqlite_table_exists(conn: &rusqlite::Connection, table: &str) -> Result<bool, SqliteError> {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
        rusqlite::params![table],
        |row| row.get::<_, i64>(0),
    )
    .optional()
    .map(|row| row.is_some())
    .map_err(SqliteError::Rusqlite)
}

/// Populate `table`'s [`text::rowid_map_table`] from `table` itself, and
/// record completion in [`text::rowid_map_state_table`], the first time this
/// backend opens a database that predates the map.
///
/// `StorageBackend::text()`/`text_with_tokenizer()` is called uncached on
/// essentially every text-store access (`khive-runtime` builds a fresh
/// `Fts5TextSearch` per call, never caching the `Arc`). The backend skips this
/// check once a table's marker is durable, but a table with no rows yet is
/// re-checked on every fetch, so the check stays on the hot path — it must
/// stay O(1), not scale with either
/// table's row count, or it reintroduces the exact class of cost this
/// migration exists to remove. `SELECT EXISTS(... LIMIT 1)` is an index probe
/// that stops at the first row, unlike `COUNT(*)` which SQLite satisfies by
/// walking every row of the smallest available index.
///
/// Completion is read from a durable marker row rather than inferred from
/// the map's own row count: a map can legitimately be empty for a table with
/// no rows yet, which is indistinguishable from "never backfilled" by row
/// count alone, and every runtime write path (`text.rs`'s
/// `delete_document_dml`, `upsert_document_dml`, `batch_upsert_documents_dml`,
/// and the raw SQL in `khive-runtime`'s
/// `atomic_prepare`/`atomic_message`/`curation`) maintains the FTS row and
/// its map row atomically, inside one transaction, so a legitimately
/// half-empty map from a live write path never happens either. Until the
/// table actually holds a row, there is nothing to reconcile and the marker
/// is deliberately left unwritten — both probes below stay O(1) index-only
/// lookups on an empty table, so repeating them costs nothing, and a table
/// that later gains rows through anything other than the maintained write
/// paths (a raw-SQL legacy seed, or a restored pre-map snapshot) is still
/// picked up and reconciled the next time this runs. Once the table holds at
/// least one row, the marker asserts a bijection — every live row has
/// exactly one map row pointing at it, and every map row points at a live
/// row with the same key — so reconciliation runs in three steps before the
/// marker is written, all inside one transaction:
///
/// 1. Any existing map row that no longer has a matching live FTS row at the
///    same rowid AND the same `(namespace, subject_id)` is removed first. A
///    map row can otherwise survive with the wrong key after FTS5 reuses its
///    rowid for a different document (the crash window
///    `delete_document_dml` guards against at the single-delete level; this
///    is the same class of staleness surviving into a legacy/reconciliation
///    pass instead).
/// 2. The map is (re)built from every current FTS row (`INSERT OR REPLACE`),
///    which reconciles a partially populated map rather than only filling a
///    wholly empty one. `updated_at ASC, rowid ASC` matches migration 024's
///    own backfill ordering: for any legacy duplicate `(namespace,
///    subject_id)` pair, the row with the newest `updated_at` survives
///    `INSERT OR REPLACE`, breaking a tie toward the higher rowid.
/// 3. Any non-NULL-key FTS row that lost step 2's survivor race — a
///    duplicate whose rowid the map no longer points at — is deleted,
///    mirroring migration 024's own first sweep, so no live row is left
///    without a map entry.
///
/// The marker is then written in the same transaction. Returns whether the
/// marker is durable when this returns: `false` only for a table with no rows
/// yet. This function never
/// checks `entities`/`notes` for orphaned subjects — that sweep is specific
/// to those two backing tables and stays in migration 024's SQL; a generic
/// `table_key` here has no fixed backing table to check against.
fn ensure_fts_rowid_map_backfilled(
    conn: &rusqlite::Connection,
    table: &str,
    admission: &crate::pool::WriteAdmission,
) -> Result<bool, SqliteError> {
    let map = text::rowid_map_table(table);
    let state = text::rowid_map_state_table(table);
    let already_backfilled: bool = conn.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM {state} WHERE key = 'backfill' AND value = ?1)"),
        rusqlite::params![text::ROWID_MAP_BACKFILL_COMPLETE],
        |row| row.get(0),
    )?;
    if already_backfilled {
        return Ok(true);
    }
    let fts_has_a_row: bool = conn.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM {table} LIMIT 1)"),
        [],
        |row| row.get(0),
    )?;
    if !fts_has_a_row {
        return Ok(false);
    }

    conn.execute_batch("BEGIN IMMEDIATE")?;
    if let Err(error) = admission.check() {
        return Err(crate::migrations::capacity_refusal_after_rollback(
            conn,
            conn.execute_batch("ROLLBACK"),
            error,
            "FTS rowid-map backfill",
        ));
    }
    let result: Result<(), SqliteError> = (|| {
        conn.execute_batch(&format!(
            "DELETE FROM {map} WHERE NOT EXISTS ( \
                 SELECT 1 FROM {table} \
                 WHERE {table}.rowid = {map}.rowid \
                   AND {table}.namespace = {map}.namespace \
                   AND {table}.subject_id = {map}.subject_id \
             )"
        ))?;
        conn.execute_batch(&format!(
            "INSERT OR REPLACE INTO {map} (namespace, subject_id, rowid) \
             SELECT namespace, subject_id, rowid FROM {table} \
             WHERE namespace IS NOT NULL AND subject_id IS NOT NULL \
             ORDER BY updated_at ASC, rowid ASC"
        ))?;
        conn.execute_batch(&format!(
            "DELETE FROM {table} \
             WHERE namespace IS NOT NULL AND subject_id IS NOT NULL \
               AND rowid NOT IN (SELECT rowid FROM {map})"
        ))?;
        conn.execute(
            &format!("INSERT OR REPLACE INTO {state} (key, value) VALUES ('backfill', ?1)"),
            rusqlite::params![text::ROWID_MAP_BACKFILL_COMPLETE],
        )?;
        Ok(())
    })();

    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT")?;
            Ok(true)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// Emit exactly one `tracing::warn!` for the whole process the first time
/// any table falls back to scan-fallback mode, rather than once per `text()`
/// call — `StorageBackend::text()` is called fresh on essentially every
/// access (see `ensure_fts_rowid_map_backfilled`'s doc comment), so an
/// unconditional warning here would spam the log on a hot path.
fn warn_scan_fallback_once(table: &str) {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        tracing::warn!(
            table,
            "opened a read-only text-search table with no rowid-map sidecar, or with a sidecar \
             that has never proven a completed backfill (no durable completion marker); a \
             read-only connection cannot create, backfill, or reconcile the map itself, so this \
             falls back to pre-map namespace/subject_id scan predicates for get/delete on this \
             table rather than trusting a map that might be partial"
        );
    });
}

fn validate_vector_model_key(model_key: &str) -> Result<(), SqliteError> {
    if model_key.is_empty()
        || !model_key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(SqliteError::InvalidData(format!(
            "invalid model_key '{}': must be non-empty and contain only \
             alphanumeric/underscore characters",
            model_key
        )));
    }
    Ok(())
}

fn validate_vector_table_columns(
    conn: &rusqlite::Connection,
    table: &str,
) -> Result<(), SqliteError> {
    let pragma = format!("PRAGMA table_xinfo({table})");
    let mut stmt = conn.prepare(&pragma)?;
    let mut rows = stmt.query([])?;
    let mut has_field = false;
    let mut has_embedding_model = false;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == "field" {
            has_field = true;
        }
        if name == "embedding_model" {
            has_embedding_model = true;
        }
    }
    if !has_field || !has_embedding_model {
        return Err(SqliteError::InvalidData(format!(
            "vec0 table '{table}' is missing required column(s) (field={has_field}, \
             embedding_model={has_embedding_model}); this is a pre-v0.2.8 vector schema and is \
             not supported — recreate the database"
        )));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum StoreSchemaKind {
    Entities,
    Graph,
    Notes,
    Events,
    Agents,
}

impl StoreSchemaKind {
    fn initializer(self) -> fn(&rusqlite::Connection) -> Result<(), rusqlite::Error> {
        match self {
            Self::Entities => entity::ensure_entities_schema,
            Self::Graph => graph::ensure_graph_schema,
            Self::Notes => note::ensure_notes_schema,
            Self::Events => event::ensure_events_schema,
            Self::Agents => agents::ensure_agents_schema,
        }
    }
}

#[derive(Default)]
pub(crate) struct StoreSchemaGate {
    pub(crate) ready: AtomicBool,
    #[cfg(test)]
    pub(crate) attempts: AtomicUsize,
}

impl StoreSchemaGate {
    pub(crate) fn ensure(
        &self,
        conn: &rusqlite::Connection,
        ensure: fn(&rusqlite::Connection) -> Result<(), rusqlite::Error>,
    ) -> Result<(), rusqlite::Error> {
        // A concurrent initializer or repair may have completed while this
        // caller waited for the writer. Publish readiness only after success.
        if self.ready.load(Ordering::Acquire) {
            return Ok(());
        }
        #[cfg(test)]
        self.attempts.fetch_add(1, Ordering::Relaxed);
        ensure(conn)?;
        self.ready.store(true, Ordering::Release);
        Ok(())
    }
}

/// Concrete storage backend providing capability traits.
///
/// Capability schemas are initialized once per backend. An index removed by
/// another process is repaired on the narrowly supported forced-index read paths. Other
/// external schema changes may still require reopening the backend.
pub struct StorageBackend {
    pool: Arc<ConnectionPool>,
    is_file_backed: bool,
    path: Option<std::path::PathBuf>,
    /// Vector model keys whose `vec_<model_key>` table this backend has already
    /// validated or created. A key is recorded only after the whole check
    /// succeeded under the writer, so a failed check is retried by the next
    /// fetch and a warm fetch never takes the writer. A table another process
    /// creates for a model this set does not hold is still validated before
    /// use; one that is replaced after its model was recorded is not
    /// re-checked until the backend reopens.
    vector_tables_ready: parking_lot::Mutex<std::collections::HashSet<String>>,
    /// FTS tables whose schema, rowid-map sidecar and durable backfill marker
    /// this backend has already proven under the writer. A table is recorded
    /// only once its marker exists, so a table with no rows yet is re-checked
    /// by the next fetch, and a warm fetch never takes the writer or the
    /// volume lease: a search opens its text store on every call. As with
    /// `vector_tables_ready`, a table dropped behind a live backend is not
    /// recreated until the backend is reopened.
    text_tables_ready: parking_lot::Mutex<std::collections::HashSet<String>>,
    /// How many times the lazy `notes_seq` anti-join repair has actually
    /// executed against this backend's pool. Gates `notes_for_namespace` so
    /// the repair (a full `notes` scan) runs at most once per backend for
    /// the process's lifetime instead of on every store acquisition (khive
    /// #827). Also exposed via
    /// `notes_seq_repair_run_count` for regression tests.
    notes_seq_repair_runs: AtomicUsize,
    store_schemas: [Arc<StoreSchemaGate>; 5],
}

impl StorageBackend {
    /// File-backed SQLite database.
    ///
    /// Opens (or creates) the database at `path`. An existing filesystem path
    /// whose mode is read-only is opened with the same locked-down pool
    /// configuration as [`Self::sqlite_read_only`]. The writable pool provides
    /// 1 writer + N readers in WAL mode for concurrent access.
    /// No service schema is applied — call `apply_schema()` for each service.
    /// The pool may create its internal `_khive_database_identity` singleton
    /// table on a writable open before service migrations run.
    ///
    /// The volume-lock directory is the per-user default of
    /// [`crate::default_volume_lock_dir`], shared by every process of the user;
    /// [`Self::sqlite_with_volume_lock_dir`] names one explicitly.
    pub fn sqlite(path: impl AsRef<Path>) -> Result<Self, SqliteError> {
        Self::sqlite_with_pool_config(path, PoolConfig::default(), None)
    }

    /// A private test database with a small, explicitly sized reader pool.
    #[cfg(any(test, feature = "test-support"))]
    pub fn sqlite_for_test(path: impl AsRef<Path>) -> Result<Self, SqliteError> {
        Self::sqlite_with_pool_config(path, PoolConfig::for_test(), None)
    }

    /// Open a private test database with an explicit journal mode and busy wait.
    #[cfg(any(test, feature = "test-support"))]
    pub fn sqlite_for_test_with_journal_mode(
        path: impl AsRef<Path>,
        wal_mode: bool,
        busy_timeout: std::time::Duration,
    ) -> Result<Self, SqliteError> {
        Self::sqlite_with_pool_config(
            path,
            PoolConfig {
                wal_mode,
                busy_timeout,
                write_queue_enabled: Some(true),
                ..PoolConfig::for_test()
            },
            None,
        )
    }

    /// [`Self::sqlite_for_test_with_journal_mode`] with its volume-lock files in
    /// `volume_lock_dir`. Under the test harness each directory takes its own
    /// in-process lease slot, so a fixture that holds the lease across a pause
    /// does not stall fixtures on other databases in the same test binary.
    #[cfg(any(test, feature = "test-support"))]
    pub fn sqlite_for_test_with_journal_mode_in(
        path: impl AsRef<Path>,
        wal_mode: bool,
        busy_timeout: std::time::Duration,
        volume_lock_dir: std::path::PathBuf,
    ) -> Result<Self, SqliteError> {
        Self::sqlite_with_pool_config(
            path,
            PoolConfig {
                wal_mode,
                busy_timeout,
                write_queue_enabled: Some(true),
                volume_lock_dir: Some(volume_lock_dir),
                ..PoolConfig::for_test()
            },
            None,
        )
    }

    /// Open SQLite with a reader count selected before any connections are opened.
    /// `None` preserves the default pool size and filesystem read-only detection.
    /// The volume-lock directory defaults as in [`Self::sqlite`].
    pub fn sqlite_with_max_readers(
        path: impl AsRef<Path>,
        max_readers: Option<usize>,
    ) -> Result<Self, SqliteError> {
        Self::sqlite_with_pool_config(path, PoolConfig::default(), max_readers)
    }

    /// Open a file-backed SQLite backend with its already-resolved WAL ceiling.
    /// A nonzero policy fails closed until the WAL I/O limiter is installed.
    /// The volume-lock directory defaults as in [`Self::sqlite`].
    pub fn sqlite_with_max_readers_and_wal_ceiling(
        path: impl AsRef<Path>,
        max_readers: Option<usize>,
        wal_ceiling: WalCeilingPolicy,
    ) -> Result<Self, SqliteError> {
        Self::sqlite_with_pool_config(
            path,
            PoolConfig {
                wal_ceiling,
                ..PoolConfig::default()
            },
            max_readers,
        )
    }

    pub(crate) fn sqlite_with_pool_config(
        path: impl AsRef<Path>,
        pool_config: PoolConfig,
        max_readers: Option<usize>,
    ) -> Result<Self, SqliteError> {
        crate::extension::ensure_extensions_loaded();
        let resolved = path.as_ref().to_path_buf();
        let read_only =
            std::fs::metadata(&resolved).is_ok_and(|metadata| metadata.permissions().readonly());
        let mut config = PoolConfig {
            path: Some(resolved.clone()),
            read_only,
            ..pool_config
        };
        if let Some(max_readers) = max_readers {
            config.max_readers = max_readers;
        }
        if read_only {
            config.write_queue_enabled = Some(false);
        }
        let pool = ConnectionPool::new(config)?;
        Ok(Self {
            pool: Arc::new(pool),
            is_file_backed: true,
            path: Some(resolved),
            vector_tables_ready: Default::default(),
            text_tables_ready: Default::default(),
            notes_seq_repair_runs: AtomicUsize::new(0),
            store_schemas: std::array::from_fn(|_| Arc::new(StoreSchemaGate::default())),
        })
    }

    /// File-backed SQLite database opened read-only.
    ///
    /// Opens the database at `path` and sets `PRAGMA query_only = ON` on the
    /// writer connection so that any write attempt (INSERT/UPDATE/DELETE) returns
    /// an error. Reader connections are opened with `SQLITE_OPEN_READ_ONLY` by the
    /// pool; at least one remains dedicated even for a rollback-journal snapshot,
    /// while this PRAGMA extends the protection to the otherwise-unused writer slot.
    ///
    /// The database file must already exist — unlike `sqlite()` this constructor
    /// does not create a new file.
    pub fn sqlite_read_only(path: impl AsRef<Path>) -> Result<Self, SqliteError> {
        Self::sqlite_read_only_with_pool_config(path, PoolConfig::default(), None)
    }

    /// A private read-only test database with a small reader pool.
    #[cfg(any(test, feature = "test-support"))]
    pub fn sqlite_read_only_for_test(path: impl AsRef<Path>) -> Result<Self, SqliteError> {
        Self::sqlite_read_only_with_pool_config(path, PoolConfig::for_test(), None)
    }

    /// Open a read-only SQLite store with a construction-time reader count.
    pub fn sqlite_read_only_with_max_readers(
        path: impl AsRef<Path>,
        max_readers: Option<usize>,
    ) -> Result<Self, SqliteError> {
        Self::sqlite_read_only_with_pool_config(path, PoolConfig::default(), max_readers)
    }

    /// Open a read-only SQLite backend while retaining WAL ceiling
    /// configuration for diagnostics. No writer policy is enforced.
    pub fn sqlite_read_only_with_max_readers_and_wal_ceiling(
        path: impl AsRef<Path>,
        max_readers: Option<usize>,
        wal_ceiling: WalCeilingPolicy,
    ) -> Result<Self, SqliteError> {
        Self::sqlite_read_only_with_pool_config(
            path,
            PoolConfig {
                wal_ceiling,
                ..PoolConfig::default()
            },
            max_readers,
        )
    }

    /// Open an existing live database with read-only SQLite connections.
    ///
    /// Ordinary WAL locking and change detection remain enabled, so a writable
    /// shared-memory sidecar is accepted. No schema preparation is performed.
    pub fn sqlite_read_only_live_with_max_readers_and_wal_ceiling(
        path: impl AsRef<Path>,
        max_readers: Option<usize>,
        wal_ceiling: WalCeilingPolicy,
    ) -> Result<Self, SqliteError> {
        Self::sqlite_read_only_with_pool_constructor(
            path,
            PoolConfig {
                wal_ceiling,
                ..PoolConfig::default()
            },
            max_readers,
            ConnectionPool::new_read_only_live,
        )
    }

    fn sqlite_read_only_with_pool_config(
        path: impl AsRef<Path>,
        pool_config: PoolConfig,
        max_readers: Option<usize>,
    ) -> Result<Self, SqliteError> {
        Self::sqlite_read_only_with_pool_constructor(
            path,
            pool_config,
            max_readers,
            ConnectionPool::new,
        )
    }

    fn sqlite_read_only_with_pool_constructor(
        path: impl AsRef<Path>,
        pool_config: PoolConfig,
        max_readers: Option<usize>,
        open_pool: fn(PoolConfig) -> Result<ConnectionPool, SqliteError>,
    ) -> Result<Self, SqliteError> {
        crate::extension::ensure_extensions_loaded();
        let resolved = path.as_ref().to_path_buf();
        let mut config = PoolConfig {
            path: Some(resolved.clone()),
            read_only: true,
            write_queue_enabled: Some(false),
            ..pool_config
        };
        if let Some(max_readers) = max_readers {
            config.max_readers = max_readers;
        }
        // Both pool constructors open the writer slot with `SQLITE_OPEN_READ_ONLY`
        // (no `SQLITE_OPEN_CREATE`) and set `PRAGMA query_only = ON` on it, so a
        // missing path is rejected instead of created, and any write attempt is
        // rejected at the SQLite level regardless of which code path reaches the
        // writer.
        let pool = open_pool(config)?;
        Ok(Self {
            pool: Arc::new(pool),
            is_file_backed: true,
            path: Some(resolved),
            vector_tables_ready: Default::default(),
            text_tables_ready: Default::default(),
            notes_seq_repair_runs: AtomicUsize::new(0),
            store_schemas: std::array::from_fn(|_| Arc::new(StoreSchemaGate::default())),
        })
    }

    /// In-memory SQLite database (for tests).
    ///
    /// All data is lost when the backend is dropped. The pool degrades to
    /// single-connection mode since in-memory databases cannot be shared
    /// across multiple connections.
    pub fn memory() -> Result<Self, SqliteError> {
        crate::extension::ensure_extensions_loaded();
        let config = PoolConfig {
            path: None,
            ..PoolConfig::default()
        };
        let pool = ConnectionPool::new(config)?;
        Ok(Self {
            pool: Arc::new(pool),
            is_file_backed: false,
            path: None,
            vector_tables_ready: Default::default(),
            text_tables_ready: Default::default(),
            notes_seq_repair_runs: AtomicUsize::new(0),
            store_schemas: std::array::from_fn(|_| Arc::new(StoreSchemaGate::default())),
        })
    }

    /// Apply a service's schema plan (run migrations).
    ///
    /// Each migration in the plan's `sqlite` list is applied idempotently,
    /// including when another opener commits it first. Already-applied
    /// migrations are skipped after taking the SQLite write lock. The
    /// `_schema_versions` table tracks which migrations have been run.
    pub fn apply_schema(
        &self,
        plan: &crate::migrations::ServiceSchemaPlan,
    ) -> Result<(), SqliteError> {
        let admission = self.pool.write_admission();
        crate::migrations::apply_schema_plan_with_admission(
            &mut self.pool.migration_transactions(),
            plan,
            &admission,
        )
    }

    /// Apply pack-auxiliary DDL statements.
    ///
    /// Executes the full plan in one transaction, applying each DDL statement
    /// idempotently via `execute_batch`. Each statement MUST be self-contained
    /// and use `CREATE TABLE IF NOT EXISTS` (or equivalent idempotent DDL) so
    /// that calling this method more than once does not fail.
    ///
    /// Pack auxiliary tables are NOT tracked in `_schema_versions` — they are
    /// non-versioned. Use `apply_schema` with a `ServiceSchemaPlan` when version
    /// tracking is needed.
    ///
    /// Plans declaring nullable-column upgrades must use
    /// [`Self::apply_pack_ddl_statements_with_columns`]. The runtime supplies
    /// the SQL slice and column metadata separately because its `SchemaPlan`
    /// type lives above this crate in the dependency chain.
    pub fn apply_pack_ddl_statements(
        &self,
        statements: &[&'static str],
    ) -> Result<(), SqliteError> {
        self.apply_pack_ddl_statements_with_columns(statements, &[])
    }

    /// Apply a pack's nullable-column upgrades and idempotent SQL atomically.
    ///
    /// Missing columns are added only to existing tables; the full SQL plan
    /// creates fresh tables. Existing columns and the final schema must match
    /// the declarations. Schema inspection, additions, and SQL all run under
    /// one writer transaction, including rollback if any later step fails.
    pub fn apply_pack_ddl_statements_with_columns(
        &self,
        statements: &[&'static str],
        additions: &[khive_types::PackColumnAddition],
    ) -> Result<(), SqliteError> {
        let writer = self.pool.writer()?;
        writer.transaction(|conn| {
            pack_schema::add_missing_columns(conn, additions)?;
            for &stmt in statements {
                conn.execute_batch(stmt)?;
            }
            pack_schema::validate_columns(conn, additions)?;
            Ok(())
        })
    }

    /// Validate a pack's declared columns without applying SQL or acquiring a writer.
    /// Read-only hosts use this before exposing verbs that require these columns.
    pub fn validate_pack_schema_columns(
        &self,
        additions: &[khive_types::PackColumnAddition],
    ) -> Result<(), SqliteError> {
        if additions.is_empty() {
            return Ok(());
        }
        let reader = self.pool.reader()?;
        pack_schema::validate_columns(reader.conn(), additions)
    }

    /// Prepare the core schema for runtime boot.
    ///
    /// Writable backends acquire the canonical database-GC owner before the
    /// writer, apply the ordinary versioned prefix, and may finish V21 only
    /// through its zero-legacy-reference fast path. A legacy V20 database
    /// remains at V20 for the async host's application-assisted attachment
    /// cutover; this method alone is not a serving boot gate.
    /// Read-only backends perform a query-only compatibility check and require
    /// the snapshot to be at this build's exact latest schema version.
    pub fn prepare_core_schema(&self) -> Result<u32, SqliteError> {
        if self.is_read_only() {
            let reader = self.pool.reader()?;
            crate::migrations::validate_schema_is_current(reader.conn())
        } else {
            let latest = crate::migrations::MIGRATIONS
                .last()
                .map(|migration| migration.version)
                .unwrap_or(0);
            {
                let reader = self.pool.reader()?;
                let current = crate::migrations::read_schema_version(reader.conn())?;
                if current >= latest {
                    return crate::migrations::validate_schema_is_current(reader.conn());
                }
            }
            let owner = crate::stores::blob::acquire_database_gc_owner_for_path_blocking(
                self.pool.canonical_path().map(Path::to_path_buf),
            )
            .map_err(|error| {
                SqliteError::InvalidData(format!(
                    "failed to acquire database GC owner before schema preparation: {error}"
                ))
            })?;
            self.run_core_migrations(&owner)
        }
    }

    /// Inspect the coordinated V21 attachment cutover state.
    pub fn attachment_cutover_status(
        &self,
    ) -> Result<crate::migrations::AttachmentCutoverStatus, SqliteError> {
        let reader = self.pool.reader()?;
        crate::migrations::attachment_cutover_status(reader.conn())
    }

    fn require_attachment_cutover_owner(
        &self,
        owner: &crate::stores::blob::DatabaseGcOwnerGuard,
    ) -> Result<(), SqliteError> {
        let sql = self.sql();
        let backend_path = sql.database_path();
        if owner.database_path() != backend_path.as_deref() {
            return Err(SqliteError::InvalidData(format!(
                "attachment cutover GC owner targets {:?}, but this backend is {:?}",
                owner.database_path(),
                backend_path.as_deref()
            )));
        }
        Ok(())
    }

    /// Commit resumable V21 stage 1 while the caller owns this database's GC
    /// protocol. The owner must remain live through verified application
    /// backfill and finalization.
    pub fn stage_attachment_cutover(
        &self,
        owner: &crate::stores::blob::DatabaseGcOwnerGuard,
    ) -> Result<(), SqliteError> {
        self.require_attachment_cutover_owner(owner)?;
        if self.is_read_only() {
            return Err(SqliteError::InvalidData(
                "cannot stage attachment cutover on a read-only backend".into(),
            ));
        }
        let mut writer = self.pool.writer()?;
        let admission = self.pool.write_admission();
        crate::migrations::stage_attachment_cutover_with_admission(writer.conn_mut(), &admission)
    }

    /// Atomically publish a verified batch of pack-owned attachment roles.
    pub fn apply_verified_attachments(
        &self,
        owner: &crate::stores::blob::DatabaseGcOwnerGuard,
        attachments: &[khive_storage::Attachment],
    ) -> Result<(), SqliteError> {
        self.require_attachment_cutover_owner(owner)?;
        if self.is_read_only() {
            return Err(SqliteError::InvalidData(
                "cannot apply verified attachments on a read-only backend".into(),
            ));
        }
        let mut writer = self.pool.writer()?;
        let tx = writer
            .conn_mut()
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let admission = self.pool.write_admission();
        if let Err(error) = admission.check() {
            let rollback = tx.rollback();
            return Err(crate::migrations::capacity_refusal_after_rollback(
                writer.conn(),
                rollback,
                error,
                "verified attachment publication",
            ));
        }
        for attachment in attachments {
            attachment
                .validate()
                .map_err(|error| SqliteError::InvalidData(error.to_string()))?;
            crate::migrations::apply_generic_verified_attachment(
                &tx,
                &attachment.record_uuid.to_string(),
                attachment.substrate.as_str(),
                &attachment.role,
                &attachment.content_ref,
                attachment.media_type.as_deref(),
                attachment.size_bytes,
                attachment.created_at,
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Atomically swap GC liveness/fences to attachments, remove the legacy
    /// entity column, and record V21 while the canonical owner is held.
    pub fn finalize_attachment_cutover(
        &self,
        owner: &crate::stores::blob::DatabaseGcOwnerGuard,
    ) -> Result<(), SqliteError> {
        self.require_attachment_cutover_owner(owner)?;
        if self.is_read_only() {
            return Err(SqliteError::InvalidData(
                "cannot finalize attachment cutover on a read-only backend".into(),
            ));
        }
        let mut writer = self.pool.writer()?;
        let admission = self.pool.write_admission();
        crate::migrations::finalize_attachment_cutover_with_admission(writer.conn_mut(), &admission)
    }

    /// Get an EntityStore. Applies the entities DDL if not already present.
    ///
    /// Idempotent — safe to call multiple times.
    pub fn entities(&self) -> Result<Arc<dyn khive_storage::EntityStore>, SqliteError> {
        self.entities_for_namespace("local")
    }

    /// Get an EntityStore. The namespace parameter is validated (non-empty) and
    /// the entities schema is applied, but the store itself is unscoped — namespace
    /// is the caller's responsibility on each query/delete call.
    pub fn entities_for_namespace(
        &self,
        namespace: &str,
    ) -> Result<Arc<dyn khive_storage::EntityStore>, SqliteError> {
        if namespace.trim().is_empty() {
            return Err(SqliteError::InvalidData(
                "entities namespace must be non-empty".to_string(),
            ));
        }
        self.ensure_store_schema(StoreSchemaKind::Entities)?;

        Ok(Arc::new(entity::SqlEntityStore::new(
            Arc::clone(&self.pool),
            self.is_file_backed,
        )))
    }

    /// Get the role-keyed attachment store.
    ///
    /// Unlike the legacy capability accessors, this does not install DDL on
    /// demand. The coordinated V21 core cutover owns creation of the table,
    /// reference fences, GC liveness swap, and removal of the legacy entity
    /// column as one boot-gated operation.
    pub fn attachments(&self) -> Result<Arc<dyn khive_storage::AttachmentStore>, SqliteError> {
        Ok(Arc::new(attachment::SqlAttachmentStore::new(
            Arc::clone(&self.pool),
            self.is_file_backed,
        )))
    }

    /// Get a GraphStore for the default namespace.
    ///
    /// Creates the `graph_edges` table (with indexes) if it does not already
    /// exist. Idempotent — safe to call multiple times.
    pub fn graph(&self) -> Result<Arc<dyn khive_storage::GraphStore>, SqliteError> {
        self.graph_for_namespace("local")
    }

    /// Get a GraphStore scoped to a namespace.
    pub fn graph_for_namespace(
        &self,
        namespace: &str,
    ) -> Result<Arc<dyn khive_storage::GraphStore>, SqliteError> {
        if namespace.trim().is_empty() {
            return Err(SqliteError::InvalidData(
                "graph namespace must be non-empty".to_string(),
            ));
        }
        self.ensure_store_schema(StoreSchemaKind::Graph)?;

        Ok(Arc::new(
            graph::SqlGraphStore::new_scoped(
                Arc::clone(&self.pool),
                self.is_file_backed,
                namespace.trim().to_string(),
            )
            .with_index_repair(crate::stores::index_repair::IndexRepairContext::new(
                Arc::clone(&self.pool),
                self.store_schemas.clone(),
                crate::stores::index_repair::IndexReadKind::Graph,
            )),
        ))
    }

    fn ensure_store_schema(&self, kind: StoreSchemaKind) -> Result<(), SqliteError> {
        if self.is_read_only()
            || self.store_schemas[kind as usize]
                .ready
                .load(Ordering::Acquire)
        {
            return Ok(());
        }
        let writer = self.constructor_writer()?;
        self.ensure_store_schema_with_writer(kind, writer.conn())
    }

    fn ensure_store_schema_with_writer(
        &self,
        kind: StoreSchemaKind,
        conn: &rusqlite::Connection,
    ) -> Result<(), SqliteError> {
        self.store_schemas[kind as usize].ensure(conn, kind.initializer())?;
        Ok(())
    }

    fn constructor_writer(
        &self,
    ) -> Result<crate::pool::PooledAutocommitWriteUnit<'_>, SqliteError> {
        let context = khive_storage::capture_request_read_context();
        let Some(operation) = context.store_acquisition_operation() else {
            return self.pool.autocommit_write_unit();
        };
        let writer = self
            .pool
            .writer_until_for_admitted_operation(|| context.blocking_stop_reason().is_some())?
            .ok_or_else(|| {
                SqliteError::RequestReadStopped(khive_storage::StorageError::Timeout {
                    operation: operation.into(),
                })
            })?;
        writer.admit_autocommit()
    }

    /// Get a NoteStore. Applies the notes DDL if not already present.
    ///
    /// Idempotent — safe to call multiple times.
    pub fn notes(&self) -> Result<Arc<dyn khive_storage::NoteStore>, SqliteError> {
        self.notes_for_namespace("local")
    }

    /// Get a NoteStore. The namespace parameter is validated (non-empty) and
    /// the notes schema is applied, but the store itself is unscoped — namespace
    /// is the caller's responsibility on each query/delete call.
    pub fn notes_for_namespace(
        &self,
        namespace: &str,
    ) -> Result<Arc<dyn khive_storage::NoteStore>, SqliteError> {
        if namespace.trim().is_empty() {
            return Err(SqliteError::InvalidData(
                "notes namespace must be non-empty".to_string(),
            ));
        }
        if !self.is_read_only()
            && (!self.store_schemas[StoreSchemaKind::Notes as usize]
                .ready
                .load(Ordering::Acquire)
                || self.notes_seq_repair_runs.load(Ordering::Relaxed) == 0)
        {
            let writer = self.constructor_writer()?;
            self.ensure_store_schema_with_writer(StoreSchemaKind::Notes, writer.conn())?;

            // The anti-join repair is a full `notes` scan -- gate it to run at
            // most once per backend/pool. `try_writer()` blocks for exclusive
            // access to the single writer connection for this whole function,
            // so this load-then-run-then-store is race-free: no other caller on
            // this pool can observe or advance `notes_seq_repair_runs` while we
            // hold the writer guard (khive #827).
            if self.notes_seq_repair_runs.load(Ordering::Relaxed) == 0 {
                note::repair_notes_seq(writer.conn())?;
                self.notes_seq_repair_runs.fetch_add(1, Ordering::Relaxed);
            }
        }

        Ok(Arc::new(
            note::SqlNoteStore::new(Arc::clone(&self.pool), self.is_file_backed).with_index_repair(
                crate::stores::index_repair::IndexRepairContext::new(
                    Arc::clone(&self.pool),
                    self.store_schemas.clone(),
                    crate::stores::index_repair::IndexReadKind::Notes,
                ),
            ),
        ))
    }

    /// How many times the lazy `notes_seq` anti-join repair has actually
    /// executed against this backend's pool. Exposed for regression tests
    /// asserting the repair runs at most once per backend for the process's
    /// lifetime, not once per `notes_for_namespace` call (khive #827).
    pub fn notes_seq_repair_run_count(&self) -> usize {
        self.notes_seq_repair_runs.load(Ordering::Relaxed)
    }

    /// Get an EventStore for the default namespace.
    ///
    /// Creates the `events` table (with indexes) if it does not already exist.
    /// Idempotent — safe to call multiple times.
    pub fn events(&self) -> Result<Arc<dyn khive_storage::EventStore>, SqliteError> {
        self.events_for_namespace("local")
    }

    /// Get an EventStore scoped to a namespace.
    pub fn events_for_namespace(
        &self,
        namespace: &str,
    ) -> Result<Arc<dyn khive_storage::EventStore>, SqliteError> {
        if namespace.trim().is_empty() {
            return Err(SqliteError::InvalidData(
                "events namespace must be non-empty".to_string(),
            ));
        }
        self.ensure_store_schema(StoreSchemaKind::Events)?;

        Ok(Arc::new(event::SqlEventStore::new_scoped(
            Arc::clone(&self.pool),
            self.is_file_backed,
            namespace.trim().to_string(),
        )))
    }

    /// Get the agent-process store (ADR-142 §1). Applies the agents DDL if not
    /// already present. Idempotent — safe to call multiple times. Unlike the
    /// other stores here, agent-process records are not namespace-scoped, so
    /// there is no `_for_namespace` variant.
    pub fn agents(&self) -> Result<Arc<dyn khive_storage::AgentStore>, SqliteError> {
        self.ensure_store_schema(StoreSchemaKind::Agents)?;

        Ok(Arc::new(agents::SqlAgentStore::new(
            Arc::clone(&self.pool),
            self.is_file_backed,
        )))
    }

    /// Get a VectorStore for a specific embedding model, scoped to the default namespace.
    ///
    /// Creates the vec0 virtual table if it does not already exist. The `model_key`
    /// must contain only ASCII alphanumeric/underscore characters. The `embedding_model`
    /// is the canonical display name stored in each vector row.
    pub fn vectors(
        &self,
        model_key: &str,
        embedding_model: &str,
        dimensions: usize,
    ) -> Result<Arc<dyn khive_storage::VectorStore>, SqliteError> {
        self.vectors_for_namespace(model_key, embedding_model, dimensions, "local")
    }

    /// Get a VectorStore for a specific embedding model with a default namespace.
    ///
    /// Creates the vec0 virtual table if it does not already exist. The `namespace`
    /// is a default for trait methods that lack a per-call namespace parameter
    /// (count, delete, info). Access control is enforced at the runtime layer.
    ///
    /// The `model_key` must contain only ASCII alphanumeric/underscore characters.
    /// The `embedding_model` is the canonical display name stored in the `embedding_model`
    /// column of each vector row (e.g. `"all-minilm-l6-v2"`).
    pub fn vectors_for_namespace(
        &self,
        model_key: &str,
        embedding_model: &str,
        dimensions: usize,
        namespace: &str,
    ) -> Result<Arc<dyn khive_storage::VectorStore>, SqliteError> {
        validate_vector_model_key(model_key)?;
        if namespace.trim().is_empty() {
            return Err(SqliteError::InvalidData(
                "vector store namespace must be non-empty".to_string(),
            ));
        }
        self.ensure_vector_tables(&[(model_key, dimensions)])?;
        Ok(Arc::new(vectors::SqliteVecStore::new(
            Arc::clone(&self.pool),
            self.is_file_backed,
            model_key.to_string(),
            embedding_model.to_string(),
            dimensions,
            namespace.trim().to_string(),
        )?))
    }

    /// Ensure all requested vector tables with one schema-writer acquisition.
    /// Read-only backends inspect the same tables using one reader instead.
    ///
    /// A writable backend prepares each model's table once: the first call that
    /// names it validates or creates the table under the writer and records the
    /// model only after every step succeeded. Later calls for a recorded model
    /// return without taking the writer, and a failed call records nothing, so
    /// the next call repeats the check.
    pub fn ensure_vector_tables(&self, models: &[(&str, usize)]) -> Result<(), SqliteError> {
        for (model_key, _) in models {
            validate_vector_model_key(model_key)?;
        }
        if models.is_empty() {
            return Ok(());
        }

        // Ensure sqlite-vec is registered before creating vec0 tables.
        crate::extension::ensure_extensions_loaded();

        // Direct store construction may precede migrations. Leave the
        // provenance sidecar to its versioned migration; vector operations
        // tolerate its absence until the ledger advances.

        if self.is_read_only() {
            // Snapshot inspection must not check schema through the pool's
            // query-only writer slot: even a SELECT there is a writer-class
            // acquisition and violates ADR-028 A2's write-free lifecycle.
            let reader = self.pool.reader()?;
            for (model_key, _) in models {
                let table = format!("vec_{model_key}");
                if !sqlite_table_exists(reader.conn(), &table)? {
                    return Err(SqliteError::InvalidData(format!(
                        "read-only database has no vector table '{table}'; create and populate it in \
                         a writable copy before opening the snapshot"
                    )));
                }
                validate_vector_table_columns(reader.conn(), &table)?;
            }
            return Ok(());
        }

        let pending = self.unprepared_vector_tables(models);
        if pending.is_empty() {
            return Ok(());
        }
        let writer = self.constructor_writer()?;
        // A concurrent caller may have prepared these tables while this one
        // waited for the writer.
        let pending = self.unprepared_vector_tables(&pending);
        if pending.is_empty() {
            return Ok(());
        }

        // Detect old-schema vec0 tables that predate the `field` column.
        // Use pragma_table_info to check columns directly; substring matching on the
        // CREATE DDL is fragile (a model_key containing "field" would false-match).
        for (model_key, _) in &pending {
            let table = format!("vec_{model_key}");
            // V17 migration (vector_embedding_model_tag_preserving_rebuild) adds
            // `field` and `embedding_model` to all pre-existing vec0 tables at
            // migration time.  If this table still lacks either column post-migration
            // that indicates the database was not migrated — return a hard error
            // rather than silently dropping data.
            if sqlite_table_exists(writer.conn(), &table)? {
                validate_vector_table_columns(writer.conn(), &table)?;
            }
        }

        // Ensure the _embedding_models registry table exists.
        // This is a no-op when the table already exists. Running it here ensures
        // the registry is present for any caller that opens a vector store without
        // first calling run_migrations() (e.g., tests that create stores directly).
        // Production callers are expected to call run_migrations() at startup, which
        // creates the registry via V14; this is a belt-and-suspenders fallback.
        // Schema is defined in `migrations::EMBEDDING_MODELS_DDL` (single source of
        // truth) to prevent the two copies from silently drifting.
        writer
            .conn()
            .execute_batch(crate::migrations::EMBEDDING_MODELS_DDL)?;

        // Same guarantee for the ANN write log: vector write paths append to it
        // in the same transaction as the vec0 mutation, so it must exist in any
        // database that hosts vec_* tables.
        writer
            .conn()
            .execute_batch(crate::migrations::ANN_WRITE_LOG_DDL)?;
        writer
            .conn()
            .execute_batch(crate::migrations::ANN_WRITE_LOG_MODEL_SEQ_INDEX_DDL)?;
        writer
            .conn()
            .execute_batch(crate::migrations::ANN_CONSUMER_PENDING_DDL)?;
        // Create missing vec0 tables without changing existing vector data.
        for (model_key, dimensions) in &pending {
            let ddl = format!(
                "CREATE VIRTUAL TABLE IF NOT EXISTS vec_{} USING vec0(\
                 subject_id TEXT PRIMARY KEY, \
                 namespace TEXT NOT NULL, \
                 kind TEXT NOT NULL, \
                 field TEXT NOT NULL, \
                 embedding_model TEXT NOT NULL, \
                 embedding float[{}] distance_metric=cosine\
                 )",
                model_key, dimensions
            );
            writer.conn().execute_batch(&ddl)?;
        }
        // Record the models only now that every step above succeeded, while the
        // writer is still held so a waiting caller re-checks against this set.
        self.vector_tables_ready.lock().extend(
            pending
                .iter()
                .map(|(model_key, _)| (*model_key).to_string()),
        );
        Ok(())
    }

    /// The requested vector models this backend has not yet validated or created.
    fn unprepared_vector_tables<'a>(&self, models: &[(&'a str, usize)]) -> Vec<(&'a str, usize)> {
        let ready = self.vector_tables_ready.lock();
        models
            .iter()
            .filter(|(model_key, _)| !ready.contains(*model_key))
            .copied()
            .collect()
    }

    /// Register an embedding model in the `_embedding_models` registry table.
    ///
    /// Idempotent: if a row with the same `canonical_key` already exists, updates its
    /// status back to `'active'` without changing other fields.
    pub fn register_embedding_model(
        &self,
        engine_name: &str,
        model_id: &str,
        key_version: &str,
        dimensions: u32,
    ) -> Result<(), SqliteError> {
        let writer = self.pool.autocommit_write_unit()?;
        writer
            .conn()
            .execute_batch(crate::migrations::EMBEDDING_MODELS_DDL)
            .map_err(|error| {
                SqliteError::write(error, khive_storage::error::SqliteWriteStage::Statement)
            })?;

        let now = chrono::Utc::now().timestamp_micros();
        let canonical_key =
            format!("{engine_name}:{model_id}:{key_version}:{dimensions}").into_bytes();
        let id = uuid::Uuid::new_v4();
        writer
            .conn()
            .execute(
                "INSERT INTO _embedding_models \
             (id, engine_name, model_id, key_version, dim, output_dim, status, \
              activated_at, superseded_at, superseded_by, canonical_key, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, NULL, 'active', ?6, NULL, NULL, ?7, ?8) \
             ON CONFLICT(canonical_key) DO UPDATE SET \
                status = 'active', \
                activated_at = COALESCE(_embedding_models.activated_at, excluded.activated_at)",
                rusqlite::params![
                    id.as_bytes().as_slice(),
                    engine_name,
                    model_id,
                    key_version,
                    dimensions as i64,
                    now,
                    canonical_key,
                    now,
                ],
            )
            .map_err(|error| {
                SqliteError::write(error, khive_storage::error::SqliteWriteStage::Statement)
            })?;
        Ok(())
    }

    /// Get a SparseStore for a specific model key, scoped to the default namespace.
    ///
    /// Creates the sparse table if it does not already exist.
    pub fn sparse(
        &self,
        model_key: &str,
    ) -> Result<Arc<dyn khive_storage::SparseStore>, SqliteError> {
        self.sparse_for_namespace(model_key, "local")
    }

    /// Get a SparseStore for a specific model key with an explicit default namespace.
    ///
    /// The `model_key` must contain only ASCII alphanumeric/underscore characters.
    pub fn sparse_for_namespace(
        &self,
        model_key: &str,
        namespace: &str,
    ) -> Result<Arc<dyn khive_storage::SparseStore>, SqliteError> {
        if model_key.is_empty()
            || !model_key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(SqliteError::InvalidData(format!(
                "invalid model_key '{}': must be non-empty and contain only alphanumeric/underscore characters",
                model_key
            )));
        }
        if namespace.trim().is_empty() {
            return Err(SqliteError::InvalidData(
                "sparse store namespace must be non-empty".to_string(),
            ));
        }

        if self.is_read_only() {
            let table = format!("sparse_{model_key}");
            let reader = self.pool.reader()?;
            if !sqlite_table_exists(reader.conn(), &table)? {
                return Err(SqliteError::InvalidData(format!(
                    "read-only database has no sparse table '{table}'; create and populate it in \
                     a writable copy before opening the snapshot"
                )));
            }
        } else {
            let writer = self.pool.autocommit_write_unit()?;
            sparse::ensure_sparse_schema(writer.conn(), model_key)
                .map_err(SqliteError::Rusqlite)?;
        }

        Ok(Arc::new(sparse::SqliteSparseStore::new(
            Arc::clone(&self.pool),
            self.is_file_backed,
            model_key.to_string(),
            namespace.trim().to_string(),
        )?))
    }

    /// Get a TextSearch for a specific table key.
    ///
    /// Creates the FTS5 virtual table if it does not already exist. Uses the
    /// `trigram` tokenizer by default (CJK-safe).
    ///
    /// The `table_key` must contain only ASCII alphanumeric/underscore characters.
    pub fn text(&self, table_key: &str) -> Result<Arc<dyn khive_storage::TextSearch>, SqliteError> {
        self.text_with_tokenizer(table_key, "trigram")
    }

    /// Get a TextSearch with an explicit FTS5 tokenizer.
    ///
    /// Use when you need a tokenizer other than the default `trigram` — for
    /// example `unicode61` for Latin-only corpora.
    ///
    /// Both `table_key` and `tokenizer` must contain only ASCII
    /// alphanumeric/underscore characters.
    pub fn text_with_tokenizer(
        &self,
        table_key: &str,
        tokenizer: &str,
    ) -> Result<Arc<dyn khive_storage::TextSearch>, SqliteError> {
        if table_key.is_empty()
            || !table_key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(SqliteError::InvalidData(format!(
                "invalid table_key '{}': must be non-empty and contain only \
                 alphanumeric/underscore characters",
                table_key
            )));
        }
        // `text::rowid_map_table`/`text::rowid_map_state_table` name a
        // table's sidecar map `{table}_rowids` and that map's own completion
        // marker `{table}_rowids_state` — a `table_key` ending in either
        // reserved suffix (e.g. "entities_rowids" or "entities_rowids_state")
        // would resolve to the exact sidecar table name another key's own
        // map or marker already reserves. `CREATE VIRTUAL TABLE IF NOT
        // EXISTS` would then silently accept that ordinary (non-FTS5) table
        // as if it were this key's FTS table, and every later point
        // read/write against it would fail against the wrong schema.
        if table_key.ends_with("_rowids") || table_key.ends_with("_rowids_state") {
            return Err(SqliteError::InvalidData(format!(
                "invalid table_key '{}': must not end in '_rowids' or '_rowids_state' — those \
                 suffixes are reserved for a text table's own rowid-map sidecar and its \
                 completion-marker state table (see text::rowid_map_table, \
                 text::rowid_map_state_table)",
                table_key
            )));
        }
        if tokenizer.is_empty()
            || !tokenizer
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(SqliteError::InvalidData(format!(
                "invalid tokenizer '{}': must be non-empty and contain only \
                 alphanumeric/underscore characters",
                tokenizer
            )));
        }

        let ddl = format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS fts_{} USING fts5(\
             subject_id UNINDEXED, \
             kind UNINDEXED, \
             title, \
             body, \
             tags UNINDEXED, \
             namespace UNINDEXED, \
             metadata UNINDEXED, \
             updated_at UNINDEXED, \
             record_kind, \
             tokenize = '{}'\
             )",
            table_key, tokenizer
        );
        let table = format!("fts_{table_key}");
        if self.is_read_only() {
            let reader = self.pool.reader()?;
            if !sqlite_table_exists(reader.conn(), &table)? {
                return Err(SqliteError::InvalidData(format!(
                    "read-only database has no text-search table '{table}'; create and populate \
                     it in a writable copy before opening the snapshot"
                )));
            }
            // A read-only connection cannot create, backfill, or reconcile
            // the rowid-map sidecar: a snapshot taken before this migration
            // shipped can have the FTS table without its map at all, and a
            // snapshot taken mid-backfill (a crash, or a copy made between
            // the map's creation and its completion marker being written) can
            // have a map table that is only partially populated. Trusting an
            // unproven map here would silently hide live rows the map
            // doesn't yet know about — only a map with a durable completion
            // marker (`ROWID_MAP_BACKFILL_COMPLETE`) is safe to join against
            // read-only; anything else falls back to the pre-map scan
            // predicates rather than constructing a store whose get/delete
            // would either fail against a nonexistent sidecar or silently
            // miss rows an unreconciled map doesn't cover.
            let map = text::rowid_map_table(&table);
            let map_exists = sqlite_table_exists(reader.conn(), &map)?;
            let marker_present = if map_exists {
                let state = text::rowid_map_state_table(&table);
                sqlite_table_exists(reader.conn(), &state)?
                    && reader.conn().query_row(
                        &format!(
                            "SELECT EXISTS(SELECT 1 FROM {state} WHERE key = 'backfill' AND value = ?1)"
                        ),
                        rusqlite::params![text::ROWID_MAP_BACKFILL_COMPLETE],
                        |row| row.get(0),
                    )?
            } else {
                false
            };
            if !map_exists || !marker_present {
                warn_scan_fallback_once(&table);
                return Ok(Arc::new(text::Fts5TextSearch::new_scan_fallback(
                    Arc::clone(&self.pool),
                    self.is_file_backed,
                    table_key.to_string(),
                )));
            }
        } else if !self.text_tables_ready.lock().contains(&table) {
            let writer = self.pool.autocommit_write_unit()?;
            // A concurrent caller may have proven this table while this one
            // waited for the writer.
            if !self.text_tables_ready.lock().contains(&table) {
                writer.conn().execute_batch(&ddl)?;
                writer.conn().execute_batch(&text::rowid_map_ddl(&table))?;
                let marker_durable = ensure_fts_rowid_map_backfilled(
                    writer.conn(),
                    &table,
                    &self.pool.write_admission(),
                )?;
                // Record the table only now that every step succeeded, while
                // the writer is still held.
                if marker_durable {
                    self.text_tables_ready.lock().insert(table.clone());
                }
            }
        }

        Ok(Arc::new(text::Fts5TextSearch::new(
            Arc::clone(&self.pool),
            self.is_file_backed,
            table_key.to_string(),
        )))
    }

    /// Get a `BlobStore` rooted per khive#292's precedence chain:
    /// `KHIVE_BLOB_ROOT` env var > `config_root` (a caller-resolved
    /// `khive.toml` override — `khive-db` has no TOML parser of its own) >
    /// beside this backend's database directory. `floor_bytes` overrides the
    /// default fail-closed free-space floor (`None` keeps the default, 2% of
    /// the volume and at least 5 GiB; ADR-111 Amendment 6). Errors if none of
    /// the three roots apply — e.g. an in-memory
    /// backend with no override and no env var has nowhere to default to.
    pub fn blob_store(
        &self,
        config_root: Option<&Path>,
        floor_bytes: Option<u64>,
    ) -> Result<Arc<dyn khive_storage::BlobStore>, SqliteError> {
        Ok(Arc::new(self.fs_blob_store(config_root, floor_bytes)?))
    }

    /// The concrete store behind [`Self::blob_store`], so its resolved floor
    /// can be read back.
    pub(crate) fn fs_blob_store(
        &self,
        config_root: Option<&Path>,
        floor_bytes: Option<u64>,
    ) -> Result<blob::FsBlobStore, SqliteError> {
        let root = blob::resolve_blob_root(self.data_dir().as_deref(), config_root)?;
        let floor = blob::FsBlobStore::resolve_floor(&root, floor_bytes);
        blob::FsBlobStore::new(root, floor)
    }

    /// Resolve the filesystem blob root exactly like [`Self::blob_store`] but
    /// require it to exist instead of creating it. Snapshot runtimes wrap the
    /// returned capability so its read methods remain available while every
    /// mutator is refused.
    pub fn blob_store_read_only(
        &self,
        config_root: Option<&Path>,
        floor_bytes: Option<u64>,
    ) -> Result<Arc<dyn khive_storage::BlobStore>, SqliteError> {
        let root = blob::resolve_blob_root(self.data_dir().as_deref(), config_root)?;
        let floor = blob::FsBlobStore::resolve_floor(&root, floor_bytes);
        Ok(Arc::new(blob::FsBlobStore::open_existing(root, floor)?))
    }

    /// Is this a file-backed backend?
    pub fn is_file_backed(&self) -> bool {
        self.is_file_backed
    }

    /// Whether this backend was opened with SQLite's read-only/query-only
    /// contract, explicitly or after filesystem-mode detection.
    pub fn is_read_only(&self) -> bool {
        self.pool.config().read_only
    }

    /// Return the directory containing the backend's database file, or `None`
    /// for an in-memory backend.
    pub fn data_dir(&self) -> Option<std::path::PathBuf> {
        self.path.as_ref()?.parent().map(|p| p.to_path_buf())
    }

    /// Root directory for this database's ANN segment tree, or `None` for an
    /// in-memory backend. Derived from the database file name itself
    /// (`<db-file>.ann/` beside the file), so two databases sharing a parent
    /// directory can never adopt each other's segments or UUID maps. The
    /// suffix is appended at the `OsString` byte level — a lossy UTF-8
    /// conversion would collapse distinct non-UTF-8 filenames into one
    /// replacement-character root, breaking exactly that isolation.
    pub fn ann_root(&self) -> Option<std::path::PathBuf> {
        ann_root_for(self.path.as_ref()?)
    }

    /// Access the underlying pool (escape hatch).
    pub fn pool(&self) -> &ConnectionPool {
        &self.pool
    }

    /// Durable owner evidence for this backend, captured at pool construction.
    /// The caller must choose the effective main backend before comparing it.
    pub fn database_owner_identity(
        &self,
    ) -> Result<DatabaseOwnerIdentity, DatabaseOwnerIdentityError> {
        self.pool.database_owner_identity()
    }

    /// Refuse a secondary or copied database as the supplied database owner.
    pub fn verify_database_owner(
        &self,
        expected: &DatabaseOwnerIdentity,
    ) -> Result<(), DatabaseOwnerIdentityError> {
        self.pool.verify_database_owner(expected)
    }

    /// Clone the underlying pool Arc.
    pub fn pool_arc(&self) -> Arc<ConnectionPool> {
        Arc::clone(&self.pool)
    }
}

/// `<db-file>.ann` sibling of a database file, appended at the `OsString`
/// byte level: a lossy UTF-8 conversion would collapse distinct non-UTF-8
/// filenames into one replacement-character root, breaking the per-database
/// segment isolation that `ann_root` exists to guarantee.
fn ann_root_for(path: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut file = path.file_name()?.to_os_string();
    file.push(".ann");
    path.parent().map(|p| p.join(file))
}

#[cfg(test)]
#[path = "backend/store_accessor_tests.rs"]
mod store_accessor_tests;

#[cfg(test)]
#[path = "backend/store_accessor_index_tests.rs"]
mod store_accessor_index_tests;

#[cfg(test)]
#[path = "backend_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "backend_admission_tests.rs"]
mod admission_tests;

#[cfg(test)]
mod live_readonly_tests;

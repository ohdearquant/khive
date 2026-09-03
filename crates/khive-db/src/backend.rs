//! Concrete storage backend providing capability traits.
//!
//! `StorageBackend` owns a `ConnectionPool` and provides factory methods for all
//! ten capability traits (`SqlAccess`, `NoteStore`, `EntityStore`, `GraphStore`,
//! `EventStore`, `VectorStore`, `SparseStore`, `TextSearch`, `BlobStore`, and
//! `AttachmentStore`). File-backed for production; in-memory for tests.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use rusqlite::OptionalExtension;

use crate::error::SqliteError;
use crate::pool::{ConnectionPool, PoolConfig};
use crate::sql_bridge::SqlBridge;
use crate::stores::{agents, attachment, blob, entity, event, graph, note, sparse, text, vectors};

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
/// `Fts5TextSearch` per call, never caching the `Arc`), so the already-done
/// check runs on the hot path — it must stay O(1), not scale with either
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
/// The marker is then written in the same transaction. This function never
/// checks `entities`/`notes` for orphaned subjects — that sweep is specific
/// to those two backing tables and stays in migration 024's SQL; a generic
/// `table_key` here has no fixed backing table to check against.
fn ensure_fts_rowid_map_backfilled(
    conn: &rusqlite::Connection,
    table: &str,
) -> Result<(), SqliteError> {
    let map = text::rowid_map_table(table);
    let state = text::rowid_map_state_table(table);
    let already_backfilled: bool = conn.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM {state} WHERE key = 'backfill' AND value = ?1)"),
        rusqlite::params![text::ROWID_MAP_BACKFILL_COMPLETE],
        |row| row.get(0),
    )?;
    if already_backfilled {
        return Ok(());
    }
    let fts_has_a_row: bool = conn.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM {table} LIMIT 1)"),
        [],
        |row| row.get(0),
    )?;
    if !fts_has_a_row {
        return Ok(());
    }

    conn.execute_batch("BEGIN IMMEDIATE")?;
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
            Ok(())
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

/// Concrete storage backend providing capability traits.
pub struct StorageBackend {
    pool: Arc<ConnectionPool>,
    is_file_backed: bool,
    path: Option<std::path::PathBuf>,
    /// How many times the lazy `notes_seq` anti-join repair has actually
    /// executed against this backend's pool. Gates `notes_for_namespace` so
    /// the repair (a full `notes` scan) runs at most once per backend for
    /// the process's lifetime instead of on every store acquisition (khive
    /// #827). Also exposed via
    /// `notes_seq_repair_run_count` for regression tests.
    notes_seq_repair_runs: AtomicUsize,
}

impl StorageBackend {
    /// File-backed SQLite database.
    ///
    /// Opens (or creates) the database at `path`. An existing filesystem path
    /// whose mode is read-only is opened with the same locked-down pool
    /// configuration as [`Self::sqlite_read_only`]. The writable pool provides
    /// 1 writer + N readers in WAL mode for concurrent access.
    /// No schema is applied — call `apply_schema()` for each service.
    pub fn sqlite(path: impl AsRef<Path>) -> Result<Self, SqliteError> {
        crate::extension::ensure_extensions_loaded();
        let resolved = path.as_ref().to_path_buf();
        let read_only =
            std::fs::metadata(&resolved).is_ok_and(|metadata| metadata.permissions().readonly());
        let mut config = PoolConfig {
            path: Some(resolved.clone()),
            read_only,
            ..PoolConfig::default()
        };
        if read_only {
            config.write_queue_enabled = Some(false);
        }
        let pool = ConnectionPool::new(config)?;
        Ok(Self {
            pool: Arc::new(pool),
            is_file_backed: true,
            path: Some(resolved),
            notes_seq_repair_runs: AtomicUsize::new(0),
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
        crate::extension::ensure_extensions_loaded();
        let resolved = path.as_ref().to_path_buf();
        let config = PoolConfig {
            path: Some(resolved.clone()),
            read_only: true,
            write_queue_enabled: Some(false),
            ..PoolConfig::default()
        };
        // `ConnectionPool::new` opens the writer slot with `SQLITE_OPEN_READ_ONLY`
        // (no `SQLITE_OPEN_CREATE`) and sets `PRAGMA query_only = ON` on it, so a
        // missing path is rejected instead of created, and any write attempt is
        // rejected at the SQLite level regardless of which code path reaches the
        // writer.
        let pool = ConnectionPool::new(config)?;
        Ok(Self {
            pool: Arc::new(pool),
            is_file_backed: true,
            path: Some(resolved),
            notes_seq_repair_runs: AtomicUsize::new(0),
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
            notes_seq_repair_runs: AtomicUsize::new(0),
        })
    }

    /// Get the SQL access capability.
    ///
    /// Returns an `Arc<dyn SqlAccess>` suitable for passing to services.
    pub fn sql(&self) -> Arc<dyn khive_storage::SqlAccess> {
        Arc::new(SqlBridge::new(Arc::clone(&self.pool), self.is_file_backed))
    }

    /// Apply a service's schema plan (run migrations).
    ///
    /// Each migration in the plan's `sqlite` list is applied idempotently.
    /// Already-applied migrations are skipped. The `_schema_versions` table
    /// tracks which migrations have been run.
    pub fn apply_schema(
        &self,
        plan: &crate::migrations::ServiceSchemaPlan,
    ) -> Result<(), SqliteError> {
        let writer = self.pool.try_writer()?;
        crate::migrations::apply_schema_plan(writer.conn(), plan)
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
    /// This method is lower-level than `PackRuntime::schema_plan()` — the
    /// runtime bootstrap calls `pack.schema_plan().statements` and passes the
    /// slice here. The `SchemaPlan` type lives in `khive-runtime` (above this
    /// crate in the dep chain); this method accepts a plain `&[&'static str]`
    /// to avoid a circular dependency.
    pub fn apply_pack_ddl_statements(
        &self,
        statements: &[&'static str],
    ) -> Result<(), SqliteError> {
        let writer = self.pool.try_writer()?;
        writer.transaction(|conn| {
            for &stmt in statements {
                conn.execute_batch(stmt)?;
            }
            Ok(())
        })
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
            let mut writer = self.pool.try_writer()?;
            crate::migrations::run_migrations_with_database_gc_owner(writer.conn_mut(), &owner)
        }
    }

    /// Read the applied schema version through the pool's ordinary reader or
    /// writer, without running migrations. Unlike
    /// [`migrations::inspect_schema_version`](crate::migrations::inspect_schema_version),
    /// this goes through the already-open pool rather than a fresh boot-time
    /// snapshot connection, so it tolerates a WAL sidecar left by this same
    /// backend's own recent writes.
    pub fn schema_version(&self) -> Result<u32, SqliteError> {
        if self.is_read_only() {
            let reader = self.pool.reader()?;
            crate::migrations::read_schema_version(reader.conn())
        } else {
            let writer = self.pool.try_writer()?;
            crate::migrations::read_schema_version(writer.conn())
        }
    }

    /// Inspect the coordinated V21 attachment cutover state.
    pub fn attachment_cutover_status(
        &self,
    ) -> Result<crate::migrations::AttachmentCutoverStatus, SqliteError> {
        if self.is_read_only() {
            let reader = self.pool.reader()?;
            crate::migrations::attachment_cutover_status(reader.conn())
        } else {
            let writer = self.pool.try_writer()?;
            crate::migrations::attachment_cutover_status(writer.conn())
        }
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
        let mut writer = self.pool.try_writer()?;
        crate::migrations::stage_attachment_cutover(writer.conn_mut())
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
        let mut writer = self.pool.try_writer()?;
        let tx = writer
            .conn_mut()
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
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
        let mut writer = self.pool.try_writer()?;
        crate::migrations::finalize_attachment_cutover(writer.conn_mut())
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
        if !self.is_read_only() {
            let writer = self.pool.try_writer()?;
            entity::ensure_entities_schema(writer.conn())?;
        }

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
        if !self.is_read_only() {
            let writer = self.pool.try_writer()?;
            graph::ensure_graph_schema(writer.conn())?;
        }

        Ok(Arc::new(graph::SqlGraphStore::new_scoped(
            Arc::clone(&self.pool),
            self.is_file_backed,
            namespace.trim().to_string(),
        )))
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
        if !self.is_read_only() {
            let writer = self.pool.try_writer()?;
            note::ensure_notes_schema(writer.conn())?;

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

        Ok(Arc::new(note::SqlNoteStore::new(
            Arc::clone(&self.pool),
            self.is_file_backed,
        )))
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
        if !self.is_read_only() {
            let writer = self.pool.try_writer()?;
            event::ensure_events_schema(writer.conn())?;
        }

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
        if !self.is_read_only() {
            let writer = self.pool.try_writer()?;
            agents::ensure_agents_schema(writer.conn())?;
        }

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
        if namespace.trim().is_empty() {
            return Err(SqliteError::InvalidData(
                "vector store namespace must be non-empty".to_string(),
            ));
        }

        // Ensure sqlite-vec is registered before creating vec0 tables.
        crate::extension::ensure_extensions_loaded();

        let table = format!("vec_{}", model_key);

        if self.is_read_only() {
            // Snapshot inspection must not check schema through the pool's
            // query-only writer slot: even a SELECT there is a writer-class
            // acquisition and violates ADR-028 A2's write-free lifecycle.
            let reader = self.pool.reader()?;
            if !sqlite_table_exists(reader.conn(), &table)? {
                return Err(SqliteError::InvalidData(format!(
                    "read-only database has no vector table '{table}'; create and populate it in \
                     a writable copy before opening the snapshot"
                )));
            }
            validate_vector_table_columns(reader.conn(), &table)?;
            drop(reader);
            return Ok(Arc::new(vectors::SqliteVecStore::new(
                Arc::clone(&self.pool),
                self.is_file_backed,
                model_key.to_string(),
                embedding_model.to_string(),
                dimensions,
                namespace.trim().to_string(),
            )?));
        }

        let writer = self.pool.try_writer()?;

        // Detect old-schema vec0 tables that predate the `field` column.
        // vec0 virtual tables do not support ALTER TABLE, so we must drop and recreate
        // the table if it exists without the `field` column. Vector data is a cache —
        // callers can re-embed from the source record after the table is rebuilt.
        // Use pragma_table_info to check columns directly; substring matching on the
        // CREATE DDL is fragile (a model_key containing "field" would false-match).
        let table_exists = sqlite_table_exists(writer.conn(), &table)?;

        if table_exists {
            // V17 migration (vector_embedding_model_tag_preserving_rebuild) adds
            // `field` and `embedding_model` to all pre-existing vec0 tables at
            // migration time.  If this table still lacks either column post-migration
            // that indicates the database was not migrated — return a hard error
            // rather than silently dropping data.
            validate_vector_table_columns(writer.conn(), &table)?;
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

        // Create the vec0 virtual table. Idempotent on fresh databases and after the
        // old-schema rebuild above.
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

        Ok(Arc::new(vectors::SqliteVecStore::new(
            Arc::clone(&self.pool),
            self.is_file_backed,
            model_key.to_string(),
            embedding_model.to_string(),
            dimensions,
            namespace.trim().to_string(),
        )?))
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
        let writer = self.pool.try_writer()?;
        writer
            .conn()
            .execute_batch(crate::migrations::EMBEDDING_MODELS_DDL)?;

        let now = chrono::Utc::now().timestamp_micros();
        let canonical_key =
            format!("{engine_name}:{model_id}:{key_version}:{dimensions}").into_bytes();
        let id = uuid::Uuid::new_v4();
        writer.conn().execute(
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
        )?;
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
            let writer = self.pool.try_writer()?;
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
        } else {
            let writer = self.pool.try_writer()?;
            writer.conn().execute_batch(&ddl)?;
            writer.conn().execute_batch(&text::rowid_map_ddl(&table))?;
            ensure_fts_rowid_map_backfilled(writer.conn(), &table)?;
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
    /// default 100 GB fail-closed free-space floor (`None` keeps the
    /// default). Errors if none of the three roots apply — e.g. an in-memory
    /// backend with no override and no env var has nowhere to default to.
    pub fn blob_store(
        &self,
        config_root: Option<&Path>,
        floor_bytes: Option<u64>,
    ) -> Result<Arc<dyn khive_storage::BlobStore>, SqliteError> {
        let root = blob::resolve_blob_root(self.data_dir().as_deref(), config_root)?;
        let floor = floor_bytes.unwrap_or(blob::FsBlobStore::DEFAULT_FLOOR_BYTES);
        Ok(Arc::new(blob::FsBlobStore::new(root, floor)?))
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
        let floor = floor_bytes.unwrap_or(blob::FsBlobStore::DEFAULT_FLOOR_BYTES);
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
mod tests {
    use super::*;
    use khive_storage::types::{EdgeFilter, SqlStatement, SqlValue};
    use khive_storage::{EntityFilter, EventFilter};

    #[cfg(unix)]
    use khive_storage::test_support::freeze_snapshot_sidecars;

    #[tokio::test]
    async fn hot_path_guard_g2_file_backed_read_suite_uses_only_pooled_readers() {
        let dir = tempfile::tempdir().unwrap();
        let backend = StorageBackend::sqlite(dir.path().join("hot_path_g2.db")).unwrap();
        backend.prepare_core_schema().unwrap();

        // Construct every store named by ADR-165 Slice 2 before the counter
        // baseline. Accessor-time DDL/validation is not request read traffic.
        let entities = backend.entities().unwrap();
        let notes = backend.notes().unwrap();
        let graph = backend.graph().unwrap();
        let events = backend.events().unwrap();
        let text = backend
            .text_with_tokenizer("hot_path_g2", "unicode61")
            .unwrap();
        let agents = backend.agents().unwrap();
        let attachments = backend.attachments().unwrap();
        let sparse = backend.sparse("hot_path_g2").unwrap();
        #[cfg(feature = "vectors")]
        let vectors = backend.vectors("hot_path_g2", "test-model", 2).unwrap();
        let sql = backend.sql();

        let before = backend.pool().reader_acquisition_snapshot();
        assert_eq!(
            entities
                .count_entities("local", EntityFilter::default())
                .await
                .unwrap(),
            0
        );
        assert_eq!(notes.count_notes("local", None).await.unwrap(), 0);
        assert_eq!(graph.count_edges(EdgeFilter::default()).await.unwrap(), 0);
        assert_eq!(
            events.count_events(EventFilter::default()).await.unwrap(),
            0
        );
        assert!(text
            .get_document("local", uuid::Uuid::new_v4())
            .await
            .unwrap()
            .is_none());
        assert!(agents.get("no-such-agent").await.unwrap().is_none());
        assert!(attachments
            .get_attachment(uuid::Uuid::new_v4(), "primary")
            .await
            .unwrap()
            .is_none());
        assert_eq!(sparse.count().await.unwrap(), 0);
        #[cfg(feature = "vectors")]
        assert_eq!(vectors.count().await.unwrap(), 0);

        let mut raw = sql.reader().await.unwrap();
        assert!(matches!(
            raw.query_scalar(SqlStatement {
                sql: "SELECT 1".into(),
                params: Vec::new(),
                label: None,
            })
            .await
            .unwrap(),
            Some(SqlValue::Integer(1))
        ));

        let after = backend.pool().reader_acquisition_snapshot();
        let expected_pooled_delta = 9 + u64::from(cfg!(feature = "vectors"));
        assert_eq!(
            after.pooled_checkouts - before.pooled_checkouts,
            expected_pooled_delta,
            "each ordinary file-backed read must check out exactly one pooled reader"
        );
        assert_eq!(
            after.standalone_opens, before.standalone_opens,
            "ADR-166 G2: ordinary file-backed read verbs must not open standalone readers"
        );
        assert_eq!(after.active_pooled_checkouts, 0);
        assert_eq!(
            after.completed_pooled_checkouts - before.completed_pooled_checkouts,
            expected_pooled_delta
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sqlite_detects_chmod_read_only_snapshot_and_core_reads_succeed() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chmod_snapshot.db");
        {
            let writable = StorageBackend::sqlite(&path).expect("create writable database");
            writable
                .prepare_core_schema()
                .expect("migrate writable snapshot source");
        }

        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o444);
        std::fs::set_permissions(&path, permissions).unwrap();
        freeze_snapshot_sidecars(&path);

        let read_only = StorageBackend::sqlite(&path).expect("auto-detect read-only mode");
        assert!(read_only.is_read_only());
        assert_eq!(
            read_only.pool().config().write_queue_enabled,
            Some(false),
            "read-only boot must not attempt to spawn a writer task"
        );
        assert!(read_only
            .pool()
            .writer_task_handle()
            .expect("disabled writer task is a valid configuration")
            .is_none());
        read_only
            .prepare_core_schema()
            .expect("current snapshot validates without migration writes");

        let entities = read_only.entities().expect("entity store opens read-only");
        let graph = read_only.graph().expect("graph store opens read-only");
        let notes = read_only.notes().expect("note store opens read-only");
        let events = read_only.events().expect("event store opens read-only");
        assert_eq!(
            entities
                .count_entities("local", khive_storage::EntityFilter::default())
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            graph
                .count_edges(khive_storage::types::EdgeFilter::default())
                .await
                .unwrap(),
            0
        );
        assert_eq!(notes.count_notes("local", None).await.unwrap(), 0);
        assert_eq!(
            events
                .count_events(khive_storage::EventFilter::default())
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            read_only.notes_seq_repair_run_count(),
            0,
            "read-only store acquisition must not run the DML repair"
        );
    }

    #[test]
    fn memory_backend_creates_successfully() {
        let backend = StorageBackend::memory().expect("memory backend should create");
        assert!(!backend.is_file_backed());
    }

    #[test]
    fn file_backend_creates_successfully() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let backend = StorageBackend::sqlite(&path).expect("file backend should create");
        assert!(backend.is_file_backed());
        assert!(path.exists());
    }

    #[test]
    fn data_dir_returns_none_for_memory_backend() {
        let backend = StorageBackend::memory().expect("memory backend");
        assert!(backend.data_dir().is_none());
    }

    #[test]
    fn data_dir_returns_parent_dir_for_file_backend() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.db");
        let backend = StorageBackend::sqlite(&path).expect("file backend");
        let got = backend.data_dir().expect("file backend must return Some");
        assert_eq!(got, dir.path());
    }

    #[test]
    fn ann_root_is_database_scoped_sibling_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.db");
        let backend = StorageBackend::sqlite(&path).expect("file backend");
        let got = backend.ann_root().expect("file backend must return Some");
        assert_eq!(got, dir.path().join("data.db.ann"));
        assert!(StorageBackend::memory().unwrap().ann_root().is_none());
    }

    /// Two distinct non-UTF-8 database filenames must never share an ANN
    /// root: a lossy UTF-8 conversion collapses both to the replacement
    /// character, letting one database adopt the other's segments. Exercised
    /// on the path derivation directly — APFS (macOS CI) refuses to create
    /// files with non-UTF-8 names, so a real backend cannot be opened there.
    #[cfg(unix)]
    #[test]
    fn ann_root_distinct_for_non_utf8_filenames() {
        use std::os::unix::ffi::OsStrExt;
        let path_a = std::path::Path::new("/data").join(std::ffi::OsStr::from_bytes(b"\xff.db"));
        let path_b = std::path::Path::new("/data").join(std::ffi::OsStr::from_bytes(b"\xfe.db"));
        let root_a = ann_root_for(&path_a).expect("Some for a file path");
        let root_b = ann_root_for(&path_b).expect("Some for a file path");
        assert_ne!(
            root_a, root_b,
            "distinct database files must map to distinct ANN roots"
        );
    }

    #[tokio::test]
    async fn sql_access_memory_roundtrip() {
        let backend = StorageBackend::memory().unwrap();
        let sql = backend.sql();

        let mut writer = sql.writer().await.unwrap();
        writer
            .execute_script(
                "CREATE TABLE test_rt (id TEXT PRIMARY KEY, value INTEGER NOT NULL)".into(),
            )
            .await
            .unwrap();

        let affected = writer
            .execute(SqlStatement {
                sql: "INSERT INTO test_rt (id, value) VALUES (?1, ?2)".into(),
                params: vec![SqlValue::Text("row1".into()), SqlValue::Integer(42)],
                label: None,
            })
            .await
            .unwrap();
        assert_eq!(affected, 1);

        let mut reader = sql.reader().await.unwrap();
        let row = reader
            .query_row(SqlStatement {
                sql: "SELECT id, value FROM test_rt WHERE id = ?1".into(),
                params: vec![SqlValue::Text("row1".into())],
                label: None,
            })
            .await
            .unwrap();

        let row = row.expect("should find the inserted row");
        assert_eq!(row.columns.len(), 2);
        match &row.columns[0].value {
            SqlValue::Text(s) => assert_eq!(s, "row1"),
            other => panic!("expected Text, got {other:?}"),
        }
        match &row.columns[1].value {
            SqlValue::Integer(v) => assert_eq!(*v, 42),
            other => panic!("expected Integer, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn sql_access_file_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_roundtrip.db");
        let backend = StorageBackend::sqlite(&path).unwrap();
        let sql = backend.sql();

        let mut writer = sql.writer().await.unwrap();
        writer
            .execute_script("CREATE TABLE test_f (k TEXT PRIMARY KEY, v TEXT)".into())
            .await
            .unwrap();
        writer
            .execute(SqlStatement {
                sql: "INSERT INTO test_f (k, v) VALUES (?1, ?2)".into(),
                params: vec![
                    SqlValue::Text("hello".into()),
                    SqlValue::Text("world".into()),
                ],
                label: None,
            })
            .await
            .unwrap();

        let mut reader = sql.reader().await.unwrap();
        let rows = reader
            .query_all(SqlStatement {
                sql: "SELECT k, v FROM test_f".into(),
                params: vec![],
                label: None,
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        match &rows[0].columns[1].value {
            SqlValue::Text(s) => assert_eq!(s, "world"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn sqlite_read_only_missing_path_does_not_create_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing_ro.db");
        assert!(!path.exists());

        let result = StorageBackend::sqlite_read_only(&path);
        assert!(
            result.is_err(),
            "opening a missing path read-only must fail"
        );
        assert!(
            !path.exists(),
            "opening a missing path read-only must not create the file"
        );
    }

    #[test]
    fn sqlite_read_only_sparse_store_requires_existing_table_without_writer_acquisition() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ro_sparse_tables.db");
        {
            let writable = StorageBackend::sqlite(&path).unwrap();
            writable
                .prepare_core_schema()
                .expect("migrate snapshot source");
            writable
                .sparse("present")
                .expect("create the optional sparse table while writable");
        }
        #[cfg(unix)]
        freeze_snapshot_sidecars(&path);

        let read_only = StorageBackend::sqlite_read_only(&path).unwrap();
        read_only
            .prepare_core_schema()
            .expect("validate exact current migration ledger");
        read_only
            .sparse("present")
            .expect("an existing sparse table must open read-only");
        let missing = match read_only.sparse("missing") {
            Ok(_) => panic!("a missing sparse table must fail during store acquisition"),
            Err(error) => error,
        };
        assert!(
            missing.to_string().contains("sparse_missing"),
            "the diagnostic must name the absent table: {missing}"
        );
        assert_eq!(
            read_only.pool().writer_acquisition_snapshot(),
            crate::pool::WriterAcquisitionSnapshot::default(),
            "construction, exact-ledger validation, and optional sparse-table inspection must \
             use reader connections only"
        );
    }

    #[test]
    fn sqlite_read_only_text_store_requires_existing_table_without_writer_acquisition() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ro_text_tables.db");
        {
            let writable = StorageBackend::sqlite(&path).unwrap();
            writable
                .prepare_core_schema()
                .expect("migrate snapshot source");
            writable
                .text("present")
                .expect("create the optional FTS table while writable");
        }
        #[cfg(unix)]
        freeze_snapshot_sidecars(&path);

        let read_only = StorageBackend::sqlite_read_only(&path).unwrap();
        read_only
            .prepare_core_schema()
            .expect("validate exact current migration ledger");
        read_only
            .text("present")
            .expect("an existing FTS table must open read-only");
        let missing = match read_only.text("missing") {
            Ok(_) => panic!("a missing FTS table must fail during store acquisition"),
            Err(error) => error,
        };
        assert!(
            missing.to_string().contains("fts_missing"),
            "the diagnostic must name the absent table: {missing}"
        );
        assert_eq!(
            read_only.pool().writer_acquisition_snapshot(),
            crate::pool::WriterAcquisitionSnapshot::default(),
            "construction, exact-ledger validation, and optional FTS inspection must use reader \
             connections only"
        );
    }

    #[cfg(feature = "vectors")]
    #[test]
    fn sqlite_read_only_vector_store_schema_check_uses_no_writer_acquisition() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ro_vector_tables.db");
        {
            let writable = StorageBackend::sqlite(&path).unwrap();
            writable
                .prepare_core_schema()
                .expect("migrate snapshot source");
            writable
                .vectors("present", "present", 3)
                .expect("create the optional vector table while writable");
        }
        #[cfg(unix)]
        freeze_snapshot_sidecars(&path);

        let read_only = StorageBackend::sqlite_read_only(&path).unwrap();
        read_only
            .prepare_core_schema()
            .expect("validate exact current migration ledger");
        read_only
            .vectors("present", "present", 3)
            .expect("an existing vector table must open read-only");
        assert!(
            read_only.vectors("missing", "missing", 3).is_err(),
            "a missing vector table must fail during store acquisition"
        );
        assert_eq!(
            read_only.pool().writer_acquisition_snapshot(),
            crate::pool::WriterAcquisitionSnapshot::default(),
            "construction, exact-ledger validation, and optional vector inspection must use \
             reader connections only"
        );
    }

    #[tokio::test]
    async fn sqlite_read_only_sql_writer_rejects_ddl_and_insert() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ro_writer.db");

        // Create the database and a table while writable.
        {
            let writable = StorageBackend::sqlite(&path).unwrap();
            let sql = writable.sql();
            let mut writer = sql.writer().await.unwrap();
            writer
                .execute_script("CREATE TABLE ro_existing (id INTEGER PRIMARY KEY)".into())
                .await
                .unwrap();
        }
        #[cfg(unix)]
        freeze_snapshot_sidecars(&path);

        let ro = StorageBackend::sqlite_read_only(&path).unwrap();
        let sql = ro.sql();

        // Writer acquisition itself must fail for a read-only backend.
        let writer_result = sql.writer().await;
        assert!(
            writer_result.is_err(),
            "sql().writer() must be rejected on a read-only backend"
        );
    }

    #[tokio::test]
    #[cfg(feature = "vectors")]
    async fn vectors_roundtrip_via_public_api() {
        let backend = StorageBackend::memory().unwrap();
        let store = backend.vectors("test_api", "test_api", 3).unwrap();

        let id = uuid::Uuid::new_v4();
        store
            .insert(
                id,
                khive_types::SubstrateKind::Entity,
                "local",
                "content",
                vec![vec![1.0, 0.0, 0.0]],
            )
            .await
            .unwrap();

        let hits = store
            .search(khive_storage::types::VectorSearchRequest {
                query_vectors: vec![vec![1.0, 0.0, 0.0]],
                top_k: 1,
                namespace: None,
                kind: None,
                embedding_model: None,
                filter: None,
                backend_hints: None,
            })
            .await
            .unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].subject_id, id);
        assert!(hits[0].score.to_f64() > 0.99);
    }

    #[tokio::test]
    #[cfg(feature = "vectors")]
    async fn vectors_creates_table_idempotently() {
        let backend = StorageBackend::memory().unwrap();

        let store1 = backend.vectors("idempotent", "idempotent", 3).unwrap();
        let store2 = backend.vectors("idempotent", "idempotent", 3).unwrap();

        let id = uuid::Uuid::new_v4();
        store1
            .insert(
                id,
                khive_types::SubstrateKind::Entity,
                "local",
                "content",
                vec![vec![1.0, 0.0, 0.0]],
            )
            .await
            .unwrap();

        let count = store2.count().await.unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn text_roundtrip_via_public_api() {
        let backend = StorageBackend::memory().unwrap();
        let store = backend.text("test_api").unwrap();

        let id = uuid::Uuid::new_v4();
        let doc = khive_storage::types::TextDocument {
            subject_id: id,
            kind: khive_types::SubstrateKind::Entity,
            record_kind: None,
            title: Some("Test Title".to_string()),
            body: "This is a searchable document about Rust.".to_string(),
            tags: vec!["rust".to_string()],
            namespace: "test_ns".to_string(),
            metadata: None,
            updated_at: chrono::Utc::now(),
        };
        store.upsert_document(doc).await.unwrap();

        let hits = store
            .search(khive_storage::types::TextSearchRequest {
                query: "Rust".to_string(),
                mode: khive_storage::types::TextQueryMode::Plain,
                filter: Some(khive_storage::types::TextFilter {
                    namespaces: vec!["test_ns".to_string()],
                    ..Default::default()
                }),
                top_k: 1,
                snippet_chars: 64,
            })
            .await
            .unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].subject_id, id);
        assert!(hits[0].score.to_f64() > 0.0);
    }

    #[tokio::test]
    async fn text_creates_table_idempotently() {
        let backend = StorageBackend::memory().unwrap();

        let store1 = backend.text("idempotent_fts").unwrap();
        let store2 = backend.text("idempotent_fts").unwrap();

        let id = uuid::Uuid::new_v4();
        let doc = khive_storage::types::TextDocument {
            subject_id: id,
            kind: khive_types::SubstrateKind::Note,
            record_kind: None,
            title: None,
            body: "Hello world.".to_string(),
            tags: vec![],
            namespace: "test_ns".to_string(),
            metadata: None,
            updated_at: chrono::Utc::now(),
        };
        store1.upsert_document(doc).await.unwrap();

        let count = store2
            .count(khive_storage::types::TextFilter {
                namespaces: vec!["test_ns".to_string()],
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(count, 1);
    }

    /// khive-runtime never caches the `Arc<dyn TextSearch>` `text()`/
    /// `text_for_notes()` return — every one of its ~26 call sites in
    /// `operations.rs`/`curation.rs` calls `StorageBackend::text()` fresh, so
    /// `ensure_fts_rowid_map_backfilled`'s already-backfilled short-circuit
    /// runs on essentially every text-store access. A first draft of that
    /// function used `SELECT COUNT(*)` for the short-circuit — correct, but
    /// `COUNT(*)` over an FTS5 table costs real per-call time proportional to
    /// row count (measured directly: ~130ms per call at 200,000 rows, vs
    /// ~0.01ms for the `EXISTS(...LIMIT 1)` this test pins), which would have
    /// reintroduced a scan on the read path this whole migration exists to
    /// remove. This test seeds enough rows that an O(n) short-circuit would
    /// make repeated calls visibly slow, then bounds many repeated
    /// `backend.text()` calls to a budget only a same-order-as-O(1)
    /// short-circuit can meet.
    #[tokio::test]
    async fn text_repeated_open_after_backfill_does_not_scale_with_row_count() {
        let backend = StorageBackend::memory().unwrap();
        let store = backend.text("hot_path_reopen").unwrap();

        let body = "the quick brown fox jumps over the lazy dog ".repeat(35);
        for _ in 0..5_000 {
            let doc = khive_storage::types::TextDocument {
                subject_id: uuid::Uuid::new_v4(),
                kind: khive_types::SubstrateKind::Note,
                record_kind: Some("memory".to_string()),
                title: None,
                body: body.clone(),
                tags: vec![],
                namespace: "test_ns".to_string(),
                metadata: None,
                updated_at: chrono::Utc::now(),
            };
            store.upsert_document(doc).await.unwrap();
        }

        // First call after seeding backfills the map (a real, one-time full
        // scan) — not part of what this test bounds.
        let _ = backend.text("hot_path_reopen").unwrap();

        let start = std::time::Instant::now();
        for _ in 0..500 {
            let _ = backend.text("hot_path_reopen").unwrap();
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "500 repeated backend.text() calls over a 5,000-row table took {elapsed:?} — \
             an O(1) already-backfilled short-circuit should clear this budget by a wide \
             margin; an O(row-count) short-circuit (e.g. `COUNT(*)`) would not"
        );
    }

    /// `text_repeated_open_after_backfill_...` above seeds through
    /// `upsert_document`, which already maintains the map transactionally —
    /// the map is never actually empty by the time `backend.text()` is
    /// called, so that test's short-circuit bound never exercises the real
    /// backfill body at all. This test seeds
    /// the FTS table with raw SQL, bypassing the map entirely, to reproduce
    /// a genuinely pre-migration database, then asserts the backfill that
    /// runs on the next `backend.text()` call gives every FTS row exactly
    /// one map entry (LEFT JOIN parity, both directions) before repeating
    /// the same O(1) re-open bound.
    #[tokio::test]
    async fn text_open_after_legacy_seed_backfills_the_map_with_full_parity() {
        let backend = StorageBackend::memory().unwrap();
        let table_key = "legacy_seed_parity";
        let table = format!("fts_{table_key}");
        let map = format!("{table}_rowids");

        // Establishes the schema (empty FTS table + empty map) exactly like
        // any other first call.
        let _ = backend.text(table_key).unwrap();

        // Seed rows with raw SQL directly against the FTS table, bypassing
        // `upsert_document`/the map entirely -- this is the legacy-empty-map
        // state a database predating this migration would be in.
        {
            let writer = backend.pool().writer().unwrap();
            writer.conn().execute_batch("BEGIN").unwrap();
            {
                let mut insert = writer
                    .conn()
                    .prepare(&format!(
                        "INSERT INTO {table} \
                         (subject_id, kind, title, body, tags, namespace, metadata, updated_at, \
                          record_kind) \
                         VALUES (?1, 'note', '', 'legacy body', '[]', 'test_ns', NULL, 0, 'memory')"
                    ))
                    .unwrap();
                for i in 0..500 {
                    insert
                        .execute(rusqlite::params![format!("legacy-{i}")])
                        .unwrap();
                }
            }
            writer.conn().execute_batch("COMMIT").unwrap();
        }
        {
            let writer = backend.pool().writer().unwrap();
            let map_count: i64 = writer
                .conn()
                .query_row(&format!("SELECT COUNT(*) FROM {map}"), [], |row| row.get(0))
                .unwrap();
            assert_eq!(
                map_count, 0,
                "the raw-SQL seed must bypass the map, reproducing a genuinely pre-migration db"
            );
        }

        // This call must run the REAL backfill body (not just the any-row
        // short-circuit), since the map is still empty.
        let _ = backend.text(table_key).unwrap();

        {
            let writer = backend.pool().writer().unwrap();
            let mismatched: i64 = writer
                .conn()
                .query_row(
                    &format!(
                        "SELECT \
                         (SELECT COUNT(*) FROM {table} WHERE rowid NOT IN (SELECT rowid FROM {map})) + \
                         (SELECT COUNT(*) FROM {map} WHERE rowid NOT IN (SELECT rowid FROM {table}))"
                    ),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                mismatched, 0,
                "backfill must give every FTS row exactly one map entry, both directions"
            );
            let fts_count: i64 = writer
                .conn()
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            let map_count: i64 = writer
                .conn()
                .query_row(&format!("SELECT COUNT(*) FROM {map}"), [], |row| row.get(0))
                .unwrap();
            assert_eq!(fts_count, 500);
            assert_eq!(map_count, 500);
        }

        // Now that a REAL backfill ran, repeated re-opens must still stay
        // O(1) — same budget/rationale as
        // `text_repeated_open_after_backfill_does_not_scale_with_row_count`.
        let start = std::time::Instant::now();
        for _ in 0..500 {
            let _ = backend.text(table_key).unwrap();
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "500 repeated backend.text() calls after a real backfill took {elapsed:?}"
        );
    }

    /// A map holding a row for B but none for A (the exact state a crash
    /// window predating the durable completion marker could leave behind)
    /// must be reconciled on the next writable open, not treated as already
    /// complete just because it has at least one row. Seeds the FTS table
    /// with raw SQL for both A and B, seeds the map with ONLY B's row, then
    /// resets the completion marker to reproduce a database that predates
    /// the marker's own existence, and asserts the next `backend.text()`
    /// call backfills A too.
    #[tokio::test]
    async fn text_open_reconciles_a_partial_map_instead_of_treating_it_as_complete() {
        let backend = StorageBackend::memory().unwrap();
        let table_key = "partial_map_reconcile";
        let table = format!("fts_{table_key}");
        let map = format!("{table}_rowids");
        let state = format!("{map}_state");

        // Establishes the schema (and, since both tables are still empty,
        // writes a marker for the empty case).
        let _ = backend.text(table_key).unwrap();

        let a = uuid::Uuid::new_v4();
        let b = uuid::Uuid::new_v4();
        {
            let writer = backend.pool().writer().unwrap();
            writer.conn().execute_batch("BEGIN").unwrap();
            writer
                .conn()
                .execute(
                    &format!(
                        "INSERT INTO {table} \
                         (rowid, subject_id, kind, title, body, tags, namespace, metadata, \
                          updated_at, record_kind) \
                         VALUES (1, ?1, 'note', '', 'doc a', '[]', 'test_ns', NULL, 0, 'memory')"
                    ),
                    rusqlite::params![a.to_string()],
                )
                .expect("insert A's fts row");
            writer
                .conn()
                .execute(
                    &format!(
                        "INSERT INTO {table} \
                         (rowid, subject_id, kind, title, body, tags, namespace, metadata, \
                          updated_at, record_kind) \
                         VALUES (2, ?1, 'note', '', 'doc b', '[]', 'test_ns', NULL, 0, 'memory')"
                    ),
                    rusqlite::params![b.to_string()],
                )
                .expect("insert B's fts row");
            // Only B gets a map entry -- this is the partial-map state.
            writer
                .conn()
                .execute(
                    &format!(
                        "INSERT INTO {map} (namespace, subject_id, rowid) VALUES ('test_ns', ?1, 2)"
                    ),
                    rusqlite::params![b.to_string()],
                )
                .expect("insert B's own map entry, leaving A's missing");
            // Undo the marker the schema-establishing call above wrote for
            // the then-empty table: a database whose map already predates
            // the marker mechanism entirely never has this row either.
            writer
                .conn()
                .execute(&format!("DELETE FROM {state} WHERE key = 'backfill'"), [])
                .expect("clear the completion marker");
            writer.conn().execute_batch("COMMIT").unwrap();
        }

        let store = backend.text(table_key).unwrap();

        let a_mapped: i64 = {
            let writer = backend.pool().writer().unwrap();
            writer
                .conn()
                .query_row(
                    &format!("SELECT COUNT(*) FROM {map} WHERE namespace = 'test_ns' AND subject_id = ?1"),
                    rusqlite::params![a.to_string()],
                    |row| row.get(0),
                )
                .unwrap()
        };
        assert_eq!(
            a_mapped, 1,
            "the partial map must be reconciled, not left missing A's entry"
        );

        let fetched_a = store.get_document("test_ns", a).await.unwrap();
        assert!(
            fetched_a.is_some(),
            "get_document(A) must work once the partial map is reconciled"
        );
    }

    /// A map row surviving at the right rowid but the WRONG key (the crash
    /// window `delete_document_dml`'s trailing key re-check guards against at
    /// the single-delete level, but which this reconciliation pass must also
    /// clean up if it survived into a legacy/pre-marker database) must be
    /// removed, not merely supplemented by a second, correct map row for the
    /// same rowid. Seeds the FTS table with ONE live row at rowid 7 keyed
    /// `(test_ns, B)`, seeds a stale map row `(test_ns, A, 7)` -- as if A's
    /// document once lived at rowid 7 and was replaced by B without the map
    /// being repaired -- clears the marker, and asserts the next open leaves
    /// the map with exactly `(test_ns, B, 7)`: A's stale entry gone, B's
    /// entry present, and `get_document` for each key answering accordingly.
    #[tokio::test]
    async fn text_open_removes_a_wrong_key_map_row_before_backfilling_the_right_one() {
        let backend = StorageBackend::memory().unwrap();
        let table_key = "wrong_key_map_row";
        let table = format!("fts_{table_key}");
        let map = format!("{table}_rowids");
        let state = format!("{map}_state");

        let _ = backend.text(table_key).unwrap();

        let a = uuid::Uuid::new_v4();
        let b = uuid::Uuid::new_v4();
        {
            let writer = backend.pool().writer().unwrap();
            writer.conn().execute_batch("BEGIN").unwrap();
            writer
                .conn()
                .execute(
                    &format!(
                        "INSERT INTO {table} \
                         (rowid, subject_id, kind, title, body, tags, namespace, metadata, \
                          updated_at, record_kind) \
                         VALUES (7, ?1, 'note', '', 'doc b', '[]', 'test_ns', NULL, 0, 'memory')"
                    ),
                    rusqlite::params![b.to_string()],
                )
                .expect("insert B's live fts row at rowid 7");
            writer
                .conn()
                .execute(
                    &format!(
                        "INSERT INTO {map} (namespace, subject_id, rowid) VALUES ('test_ns', ?1, 7)"
                    ),
                    rusqlite::params![a.to_string()],
                )
                .expect("insert A's stale map row still pointing at rowid 7");
            writer
                .conn()
                .execute(&format!("DELETE FROM {state} WHERE key = 'backfill'"), [])
                .expect("clear the completion marker");
            writer.conn().execute_batch("COMMIT").unwrap();
        }

        let store = backend.text(table_key).unwrap();

        let map_rows: Vec<(String, i64)> = {
            let writer = backend.pool().writer().unwrap();
            let mut stmt = writer
                .conn()
                .prepare(&format!(
                    "SELECT subject_id, rowid FROM {map} ORDER BY rowid"
                ))
                .unwrap();
            let rows = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            rows
        };
        assert_eq!(
            map_rows,
            vec![(b.to_string(), 7)],
            "the stale (A, 7) map row must be removed and replaced by the correct (B, 7) row, \
             not left alongside it"
        );

        assert!(
            store.get_document("test_ns", a).await.unwrap().is_none(),
            "A's stale map entry is gone, so get_document(A) must find nothing"
        );
        let fetched_b = store.get_document("test_ns", b).await.unwrap();
        assert!(
            fetched_b.is_some(),
            "get_document(B) must find the live row now correctly mapped"
        );
        assert_eq!(fetched_b.unwrap().body, "doc b");
    }

    /// Two non-NULL-key FTS rows for the SAME `(namespace, subject_id)` at
    /// different rowids (a pre-atomic-upsert-era duplicate) must resolve to
    /// exactly one map row at the survivor rowid, with the OTHER, losing row
    /// actually removed from the FTS table -- not merely left unmapped and
    /// invisible to every keyed lookup. Mirrors migration 024's own sweep,
    /// which this function's step 3 must reproduce at runtime.
    #[tokio::test]
    async fn text_open_sweeps_the_duplicate_that_lost_the_survivor_race() {
        let backend = StorageBackend::memory().unwrap();
        let table_key = "duplicate_loser_swept";
        let table = format!("fts_{table_key}");
        let map = format!("{table}_rowids");
        let state = format!("{map}_state");

        let _ = backend.text(table_key).unwrap();

        let dup = uuid::Uuid::new_v4();
        {
            let writer = backend.pool().writer().unwrap();
            writer.conn().execute_batch("BEGIN").unwrap();
            // Lower rowid, OLDER updated_at -- must lose the survivor race.
            writer
                .conn()
                .execute(
                    &format!(
                        "INSERT INTO {table} \
                         (rowid, subject_id, kind, title, body, tags, namespace, metadata, \
                          updated_at, record_kind) \
                         VALUES (10, ?1, 'note', '', 'older body', '[]', 'test_ns', NULL, 1, \
                          'memory')"
                    ),
                    rusqlite::params![dup.to_string()],
                )
                .expect("insert the older/losing duplicate");
            // Higher rowid, NEWER updated_at -- must survive.
            writer
                .conn()
                .execute(
                    &format!(
                        "INSERT INTO {table} \
                         (rowid, subject_id, kind, title, body, tags, namespace, metadata, \
                          updated_at, record_kind) \
                         VALUES (20, ?1, 'note', '', 'newer body', '[]', 'test_ns', NULL, 5, \
                          'memory')"
                    ),
                    rusqlite::params![dup.to_string()],
                )
                .expect("insert the newer/surviving duplicate");
            writer
                .conn()
                .execute(&format!("DELETE FROM {state} WHERE key = 'backfill'"), [])
                .expect("clear the completion marker");
            writer.conn().execute_batch("COMMIT").unwrap();
        }

        let _ = backend.text(table_key).unwrap();

        let writer = backend.pool().writer().unwrap();
        let map_rows: Vec<i64> = writer
            .conn()
            .prepare(&format!(
                "SELECT rowid FROM {map} WHERE namespace = 'test_ns' AND subject_id = ?1"
            ))
            .unwrap()
            .query_map(rusqlite::params![dup.to_string()], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            map_rows,
            vec![20],
            "exactly one map row must survive, at the newer (by updated_at) rowid"
        );

        let fts_rowids: Vec<i64> = writer
            .conn()
            .prepare(&format!("SELECT rowid FROM {table} ORDER BY rowid"))
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            fts_rowids,
            vec![20],
            "the losing duplicate (rowid 10) must be deleted from the FTS table itself, not just \
             left out of the map as an unmapped live row"
        );
    }

    /// Once `ensure_fts_rowid_map_backfilled` has written the completion
    /// marker, a later open must not re-scan the FTS table at all -- not
    /// even to reconcile it. Corrupts the map after the real backfill by
    /// deleting one of its rows directly, then asserts a second
    /// `backend.text()` call leaves that row missing: had it re-scanned, the
    /// full-table `INSERT OR REPLACE` would have restored it.
    #[tokio::test]
    async fn text_open_after_marker_written_does_not_rescan_even_a_corrupted_map() {
        let backend = StorageBackend::memory().unwrap();
        let table_key = "marker_no_rescan";
        let table = format!("fts_{table_key}");
        let map = format!("{table}_rowids");
        let state = format!("{map}_state");

        let store = backend.text(table_key).unwrap();
        store
            .upsert_document(khive_storage::types::TextDocument {
                subject_id: uuid::Uuid::new_v4(),
                kind: khive_types::SubstrateKind::Note,
                record_kind: Some("memory".to_string()),
                title: None,
                body: "seed".to_string(),
                tags: vec![],
                namespace: "test_ns".to_string(),
                metadata: None,
                updated_at: chrono::Utc::now(),
            })
            .await
            .unwrap();

        // `khive-runtime` never caches the `Arc<dyn TextSearch>` `text()`
        // returns (see this function's own doc comment) -- it calls
        // `StorageBackend::text()` fresh on essentially every access. This
        // second call is that fresh re-open: the table now holds the row
        // just written above, so `ensure_fts_rowid_map_backfilled` runs its
        // real body and writes the marker.
        let _ = backend.text(table_key).unwrap();

        let marked: bool = {
            let writer = backend.pool().writer().unwrap();
            writer
                .conn()
                .query_row(
                    &format!(
                        "SELECT EXISTS(SELECT 1 FROM {state} WHERE key = 'backfill' AND value = 'complete')"
                    ),
                    [],
                    |row| row.get(0),
                )
                .unwrap()
        };
        assert!(
            marked,
            "a completion marker must exist once the table has held a row"
        );

        {
            let writer = backend.pool().writer().unwrap();
            writer
                .conn()
                .execute(&format!("DELETE FROM {map}"), [])
                .expect("corrupt the map by deleting its row directly");
        }

        let _ = backend.text(table_key).unwrap();

        let map_count: i64 = {
            let writer = backend.pool().writer().unwrap();
            writer
                .conn()
                .query_row(&format!("SELECT COUNT(*) FROM {map}"), [], |row| row.get(0))
                .unwrap()
        };
        assert_eq!(
            map_count, 0,
            "a marker-complete table must not be re-scanned on open, even to reconcile a map \
             an external actor emptied out from under it"
        );
    }

    /// The writable legacy backfill (a table opened for the first time with
    /// FTS rows already present, predating the map entirely) must exclude
    /// NULL-key rows rather than fail the map's NOT NULL constraint.
    #[tokio::test]
    async fn text_open_writable_legacy_backfill_excludes_null_key_rows() {
        let backend = StorageBackend::memory().unwrap();
        let table_key = "legacy_null_key";
        let table = format!("fts_{table_key}");
        let map = format!("{table}_rowids");
        let state = format!("{map}_state");

        let _ = backend.text(table_key).unwrap();
        {
            let writer = backend.pool().writer().unwrap();
            writer.conn().execute_batch("BEGIN").unwrap();
            writer
                .conn()
                .execute(
                    &format!(
                        "INSERT INTO {table} \
                         (rowid, subject_id, kind, title, body, tags, namespace, metadata, \
                          updated_at, record_kind) \
                         VALUES (1, NULL, 'note', '', 'null-key body', '[]', NULL, NULL, 0, '')"
                    ),
                    [],
                )
                .expect("insert legacy NULL-key fts row");
            writer
                .conn()
                .execute(
                    &format!(
                        "INSERT INTO {table} \
                         (rowid, subject_id, kind, title, body, tags, namespace, metadata, \
                          updated_at, record_kind) \
                         VALUES (2, 'legacy-1', 'note', '', 'normal body', '[]', 'test_ns', NULL, \
                          0, 'memory')"
                    ),
                    [],
                )
                .expect("insert legacy normal-key fts row");
            writer
                .conn()
                .execute(&format!("DELETE FROM {state} WHERE key = 'backfill'"), [])
                .expect("clear the completion marker written for the then-empty table");
            writer.conn().execute_batch("COMMIT").unwrap();
        }

        // Must open without erroring against the map's NOT NULL columns.
        let _ = backend.text(table_key).unwrap();

        let writer = backend.pool().writer().unwrap();
        let map_count: i64 = writer
            .conn()
            .query_row(&format!("SELECT COUNT(*) FROM {map}"), [], |row| row.get(0))
            .unwrap();
        assert_eq!(map_count, 1, "only the non-NULL-key row may be mapped");
        let mapped_subject: String = writer
            .conn()
            .query_row(&format!("SELECT subject_id FROM {map}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(mapped_subject, "legacy-1");
    }

    /// The writable legacy backfill must choose the survivor for a
    /// duplicate `(namespace, subject_id)` key by `updated_at`, with rowid
    /// only as a tie-break -- the same `ORDER BY updated_at ASC, rowid ASC`
    /// contract migration 024 uses, not rowid alone.
    #[tokio::test]
    async fn text_open_writable_legacy_backfill_survivor_is_chosen_by_updated_at_not_rowid() {
        let backend = StorageBackend::memory().unwrap();
        let table_key = "legacy_updated_at_survivor";
        let table = format!("fts_{table_key}");
        let map = format!("{table}_rowids");
        let state = format!("{map}_state");

        let _ = backend.text(table_key).unwrap();
        {
            let writer = backend.pool().writer().unwrap();
            writer.conn().execute_batch("BEGIN").unwrap();
            // Lower rowid, but NEWER updated_at.
            writer
                .conn()
                .execute(
                    &format!(
                        "INSERT INTO {table} \
                         (rowid, subject_id, kind, title, body, tags, namespace, metadata, \
                          updated_at, record_kind) \
                         VALUES (100, 'dup', 'note', '', 'newer body', '[]', 'test_ns', NULL, \
                          500, 'memory')"
                    ),
                    [],
                )
                .expect("insert newer-but-lower-rowid fts row");
            // Higher rowid, but OLDER updated_at.
            writer
                .conn()
                .execute(
                    &format!(
                        "INSERT INTO {table} \
                         (rowid, subject_id, kind, title, body, tags, namespace, metadata, \
                          updated_at, record_kind) \
                         VALUES (200, 'dup', 'note', '', 'older body', '[]', 'test_ns', NULL, \
                          100, 'memory')"
                    ),
                    [],
                )
                .expect("insert older-but-higher-rowid fts row");
            writer
                .conn()
                .execute(&format!("DELETE FROM {state} WHERE key = 'backfill'"), [])
                .expect("clear the completion marker written for the then-empty table");
            writer.conn().execute_batch("COMMIT").unwrap();
        }

        let _ = backend.text(table_key).unwrap();

        let writer = backend.pool().writer().unwrap();
        let mapped_rowid: i64 = writer
            .conn()
            .query_row(
                &format!(
                    "SELECT rowid FROM {map} WHERE namespace = 'test_ns' AND subject_id = 'dup'"
                ),
                [],
                |row| row.get(0),
            )
            .expect("read dup's map entry");
        assert_eq!(
            mapped_rowid, 100,
            "the newer document (by updated_at) must survive even though its rowid is lower"
        );
    }

    #[test]
    fn invalid_model_key_rejected() {
        let backend = StorageBackend::memory().unwrap();
        assert!(backend.vectors("bad key!", "bad key!", 3).is_err());
        assert!(backend.vectors("", "", 3).is_err());
    }

    #[test]
    fn invalid_table_key_rejected() {
        let backend = StorageBackend::memory().unwrap();
        assert!(backend.text("bad key!").is_err());
        assert!(backend.text("").is_err());
    }

    /// A `table_key` ending in `_rowids` must be rejected outright — it
    /// would otherwise resolve to the exact sidecar
    /// table name another key's own rowid map already reserves (e.g.
    /// `"entities_rowids"` -> `fts_entities_rowids`, colliding with
    /// `"entities"`'s own map).
    #[test]
    fn table_key_ending_in_rowids_suffix_rejected() {
        let backend = StorageBackend::memory().unwrap();
        assert!(backend.text("entities_rowids").is_err());
        assert!(backend.text("notes_rowids").is_err());
        assert!(backend.text("anything_rowids").is_err());
    }

    /// The accepted case: a key that merely contains, but does not end in,
    /// the reserved suffix must still work normally.
    #[test]
    fn table_key_containing_but_not_ending_in_rowids_suffix_accepted() {
        let backend = StorageBackend::memory().unwrap();
        assert!(backend.text("rowids_but_not_at_the_end").is_ok());
    }

    /// A `table_key` ending in `_rowids_state` must be rejected outright too
    /// — it would otherwise resolve to the exact sidecar completion-marker
    /// table name another key's own rowid map already reserves (e.g.
    /// `"entities_rowids_state"` -> `fts_entities_rowids_state`, colliding
    /// with `"entities"`'s own map-state table). This suffix does not end in
    /// `_rowids`, so it needs its own check separate from the one above.
    #[test]
    fn table_key_ending_in_rowids_state_suffix_rejected() {
        let backend = StorageBackend::memory().unwrap();
        assert!(backend.text("entities_rowids_state").is_err());
        assert!(backend.text("notes_rowids_state").is_err());
        assert!(backend.text("anything_rowids_state").is_err());
    }

    /// The accepted case for the `_rowids_state` suffix: a key that merely
    /// contains, but does not end in, the reserved suffix must still work.
    #[test]
    fn table_key_containing_but_not_ending_in_rowids_state_suffix_accepted() {
        let backend = StorageBackend::memory().unwrap();
        assert!(backend.text("rowids_state_but_not_at_the_end").is_ok());
    }

    #[tokio::test]
    async fn sqlite_read_only_graph_store_rejects_upsert_edge() {
        use khive_storage::types::Edge;
        use khive_types::EdgeRelation;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ro_graph.db");

        // Create the database and the graph schema while writable.
        {
            let writable = StorageBackend::sqlite(&path).unwrap();
            writable.graph().unwrap();
        }
        #[cfg(unix)]
        freeze_snapshot_sidecars(&path);

        let ro = StorageBackend::sqlite_read_only(&path).unwrap();
        let store = match ro.graph() {
            Ok(store) => store,
            // Failing to even open the store on a read-only backend is an
            // acceptable rejection — the write path never becomes reachable.
            Err(_) => return,
        };

        let now = chrono::Utc::now();
        let edge = Edge {
            id: uuid::Uuid::new_v4().into(),
            namespace: "local".to_string(),
            source_id: uuid::Uuid::new_v4(),
            target_id: uuid::Uuid::new_v4(),
            relation: EdgeRelation::Extends,
            weight: 0.8,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            metadata: None,
            target_backend: None,
        };

        let result = store.upsert_edge(edge).await;
        assert!(
            result.is_err(),
            "upsert_edge on a read-only backend must reject, not silently no-op"
        );
    }

    #[tokio::test]
    async fn sqlite_read_only_event_store_rejects_append_event() {
        use khive_types::{EventKind, EventOutcome, SubstrateKind};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ro_events.db");

        {
            let writable = StorageBackend::sqlite(&path).unwrap();
            writable.events().unwrap();
        }
        #[cfg(unix)]
        freeze_snapshot_sidecars(&path);

        let ro = StorageBackend::sqlite_read_only(&path).unwrap();
        let store = match ro.events() {
            Ok(store) => store,
            Err(_) => return,
        };

        let event = khive_storage::event::Event::new(
            "local",
            "test.verb",
            EventKind::Audit,
            SubstrateKind::Entity,
            "test-actor",
        )
        .with_outcome(EventOutcome::Success);

        let result = store.append_event(event).await;
        assert!(
            result.is_err(),
            "append_event on a read-only backend must reject, not silently no-op"
        );
    }

    #[tokio::test]
    async fn sqlite_read_only_text_store_rejects_upsert_document() {
        use khive_storage::types::TextDocument;
        use khive_types::SubstrateKind;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ro_text.db");

        {
            let writable = StorageBackend::sqlite(&path).unwrap();
            writable.text("ro_test").unwrap();
        }
        #[cfg(unix)]
        freeze_snapshot_sidecars(&path);

        let ro = StorageBackend::sqlite_read_only(&path).unwrap();
        let store = match ro.text("ro_test") {
            Ok(store) => store,
            Err(_) => return,
        };

        let doc = TextDocument {
            subject_id: uuid::Uuid::new_v4(),
            kind: SubstrateKind::Entity,
            record_kind: None,
            title: Some("Title".to_string()),
            body: "Body text.".to_string(),
            tags: vec![],
            namespace: "local".to_string(),
            metadata: None,
            updated_at: chrono::Utc::now(),
        };

        let result = store.upsert_document(doc).await;
        assert!(
            result.is_err(),
            "upsert_document on a read-only backend must reject, not silently no-op"
        );
    }

    /// A read-only snapshot whose FTS
    /// table predates the rowid-map sidecar (created here with raw SQL,
    /// bypassing `text()`'s own map creation, to reproduce a pre-migration
    /// snapshot) must still open and serve `get_document`/`delete_document`
    /// via the scan-fallback predicates, rather than erroring against a
    /// sidecar table that was never created.
    #[tokio::test]
    async fn sqlite_read_only_text_store_without_rowid_map_falls_back_to_scan_predicates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ro_text_no_map.db");

        let id = uuid::Uuid::new_v4();
        {
            let writable = StorageBackend::sqlite(&path).unwrap();
            let writer = writable.pool().try_writer().unwrap();
            writer
                .conn()
                .execute_batch(
                    "CREATE VIRTUAL TABLE IF NOT EXISTS fts_ro_no_map USING fts5(\
                     subject_id UNINDEXED, kind UNINDEXED, title, body, tags UNINDEXED, \
                     namespace UNINDEXED, metadata UNINDEXED, updated_at UNINDEXED, \
                     record_kind, tokenize = 'trigram')",
                )
                .unwrap();
            writer
                .conn()
                .execute(
                    "INSERT INTO fts_ro_no_map \
                     (subject_id, kind, title, body, tags, namespace, metadata, updated_at, \
                      record_kind) \
                     VALUES (?1, 'note', '', 'legacy body', '[]', 'local', NULL, 0, NULL)",
                    rusqlite::params![id.to_string()],
                )
                .unwrap();
        }
        #[cfg(unix)]
        freeze_snapshot_sidecars(&path);

        let ro = StorageBackend::sqlite_read_only(&path).unwrap();
        let store = ro
            .text("ro_no_map")
            .expect("a read-only FTS table with no sidecar map must still open successfully");

        let fetched = store
            .get_document("local", id)
            .await
            .expect("scan-fallback get_document must not error against a missing map table");
        assert!(
            fetched.is_some(),
            "scan-fallback get_document must still find the legacy row"
        );
        assert_eq!(fetched.unwrap().subject_id, id);
    }

    /// A read-only snapshot whose sidecar map table EXISTS but whose
    /// completion marker was never written (the exact state a crash between
    /// the map's creation and `ensure_fts_rowid_map_backfilled` finishing
    /// could leave a copy in, since a read-only connection can never run
    /// that reconciliation itself) must still fall back to the scan
    /// predicates, not trust a map that might be partial. Seeds FTS rows for
    /// A and B directly, seeds the map (and its state sidecar, via the same
    /// DDL `text_with_tokenizer` uses) with a row for B only, and never
    /// writes the `backfill = complete` marker.
    #[tokio::test]
    async fn sqlite_read_only_text_store_with_unmarked_map_falls_back_to_scan_predicates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ro_text_unmarked_map.db");

        let a = uuid::Uuid::new_v4();
        let b = uuid::Uuid::new_v4();
        {
            let writable = StorageBackend::sqlite(&path).unwrap();
            let writer = writable.pool().try_writer().unwrap();
            writer
                .conn()
                .execute_batch(
                    "CREATE VIRTUAL TABLE IF NOT EXISTS fts_ro_unmarked USING fts5(\
                     subject_id UNINDEXED, kind UNINDEXED, title, body, tags UNINDEXED, \
                     namespace UNINDEXED, metadata UNINDEXED, updated_at UNINDEXED, \
                     record_kind, tokenize = 'trigram')",
                )
                .unwrap();
            writer
                .conn()
                .execute_batch(&text::rowid_map_ddl("fts_ro_unmarked"))
                .unwrap();
            writer
                .conn()
                .execute(
                    "INSERT INTO fts_ro_unmarked \
                     (rowid, subject_id, kind, title, body, tags, namespace, metadata, \
                      updated_at, record_kind) \
                     VALUES (1, ?1, 'note', '', 'doc a', '[]', 'local', NULL, 0, NULL)",
                    rusqlite::params![a.to_string()],
                )
                .unwrap();
            writer
                .conn()
                .execute(
                    "INSERT INTO fts_ro_unmarked \
                     (rowid, subject_id, kind, title, body, tags, namespace, metadata, \
                      updated_at, record_kind) \
                     VALUES (2, ?1, 'note', '', 'doc b', '[]', 'local', NULL, 0, NULL)",
                    rusqlite::params![b.to_string()],
                )
                .unwrap();
            // Only B gets a map entry, and the `_state` sidecar is left
            // without a `backfill` row -- no marker exists at all.
            writer
                .conn()
                .execute(
                    "INSERT INTO fts_ro_unmarked_rowids (namespace, subject_id, rowid) \
                     VALUES ('local', ?1, 2)",
                    rusqlite::params![b.to_string()],
                )
                .unwrap();
        }
        #[cfg(unix)]
        freeze_snapshot_sidecars(&path);

        let ro = StorageBackend::sqlite_read_only(&path).unwrap();
        let store = ro
            .text("ro_unmarked")
            .expect("a read-only FTS table with an unmarked map must still open successfully");

        let fetched_a = store
            .get_document("local", a)
            .await
            .expect("scan-fallback get_document must not error against an unmarked map");
        assert!(
            fetched_a.is_some(),
            "A has no map entry, so only the scan fallback (not a map join) can find it -- \
             proving the unmarked map was not trusted"
        );
        assert_eq!(fetched_a.unwrap().body, "doc a");
    }

    #[tokio::test]
    async fn blob_store_roundtrip_via_public_api() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob_backend.db");
        let backend = StorageBackend::sqlite(&path).unwrap();

        // Explicit floor_bytes=0, not the default 100GB — the free space on
        // whatever volume runs this test is not this test's concern (and a
        // dev machine or CI runner legitimately may not clear 100GB free).
        let store = backend.blob_store(None, Some(0)).unwrap();
        let bytes = b"backend-level blob roundtrip".to_vec();
        let content_ref = store.put(bytes.clone()).await.unwrap();
        assert_eq!(
            store
                .get_bounded_verified(&content_ref, bytes.len() as u64)
                .await
                .unwrap(),
            bytes
        );
    }

    #[test]
    fn blob_store_defaults_root_beside_db_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob_default.db");
        let backend = StorageBackend::sqlite(&path).unwrap();

        // `blob_store` creates the root directory eagerly (`FsBlobStore::new`),
        // so its existence at the expected default path is directly
        // observable without reaching into the trait object.
        let _store = backend.blob_store(None, None).unwrap();
        assert!(
            dir.path().join("blobs").is_dir(),
            "default root must be created beside the database file"
        );
    }

    #[test]
    fn blob_store_errors_for_in_memory_backend_with_no_override() {
        let backend = StorageBackend::memory().unwrap();
        assert!(backend.blob_store(None, None).is_err());
    }

    #[test]
    fn blob_store_accepts_explicit_root_for_in_memory_backend() {
        let dir = tempfile::tempdir().unwrap();
        let backend = StorageBackend::memory().unwrap();
        let store = backend.blob_store(Some(dir.path()), None);
        assert!(store.is_ok());
    }

    #[test]
    fn apply_schema_runs_migrations_idempotently() {
        static MIGRATIONS: &[crate::migrations::Migration] = &[crate::migrations::Migration {
            id: "001_init",
            up_sql: "CREATE TABLE IF NOT EXISTS schema_test (id TEXT PRIMARY KEY);",
            down_sql: None,
            is_already_applied: None,
        }];
        let plan = crate::migrations::ServiceSchemaPlan {
            service: "schema_test_svc",
            sqlite: MIGRATIONS,
            postgres: &[],
        };

        let backend = StorageBackend::memory().unwrap();
        backend.apply_schema(&plan).unwrap();
        backend.apply_schema(&plan).unwrap();

        let reader = backend.pool().reader().unwrap();
        let count: i64 = reader
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_test'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn pack_ddl_plan_rolls_back_all_statements_on_failure() {
        let backend = StorageBackend::memory().unwrap();
        let error = backend
            .apply_pack_ddl_statements(&[
                "CREATE TABLE IF NOT EXISTS pack_schema_first (id INTEGER PRIMARY KEY)",
                "CREATE INDEX IF NOT EXISTS pack_schema_second ON pack_schema_missing(id)",
            ])
            .unwrap_err();

        assert!(
            error.to_string().contains("pack_schema_missing"),
            "schema-plan error must retain the failing SQLite diagnostic: {error}"
        );

        let reader = backend.pool().reader().unwrap();
        let visible_objects: i64 = reader
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE name IN ('pack_schema_first', 'pack_schema_second')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(visible_objects, 0);
    }

    #[test]
    fn pack_ddl_plan_applies_all_statements_idempotently() {
        const PLAN: &[&str] = &[
            "CREATE TABLE IF NOT EXISTS pack_schema_success (id INTEGER PRIMARY KEY, value TEXT)",
            "CREATE INDEX IF NOT EXISTS pack_schema_success_value_idx \
             ON pack_schema_success(value)",
        ];

        let backend = StorageBackend::memory().unwrap();
        backend.apply_pack_ddl_statements(PLAN).unwrap();
        backend.apply_pack_ddl_statements(PLAN).unwrap();

        let reader = backend.pool().reader().unwrap();
        let visible_objects: i64 = reader
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master \
                 WHERE name IN ('pack_schema_success', 'pack_schema_success_value_idx')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(visible_objects, 2);
    }

    /// khive#1029 repro: a `create_entity`-shaped write sequence (entity
    /// upsert, then FTS `upsert_document` on the SAME file-backed DB, SAME
    /// `StorageBackend`/pool) against a fresh tenant DB file, with a short
    /// `busy_timeout` so a genuine lock hang fails fast instead of burning
    /// 30s. Runs with `write_queue_enabled: false` — the legacy pool-mutex /
    /// standalone-connection path (`KHIVE_WRITE_QUEUE` unset/0 in the
    /// hosted symptom report is one of the two configs to check; see the
    /// `_write_queue_enabled` sibling below for the flag-on config).
    fn issue_1029_pool(write_queue_enabled: bool) -> (tempfile::TempDir, StorageBackend) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("issue_1029.db");
        let config = crate::pool::PoolConfig {
            path: Some(path.clone()),
            busy_timeout: std::time::Duration::from_millis(200),
            write_queue_enabled: Some(write_queue_enabled),
            ..crate::pool::PoolConfig::default()
        };
        let pool = ConnectionPool::new(config).expect("fresh tenant-shaped pool should open");
        let backend = StorageBackend {
            pool: Arc::new(pool),
            is_file_backed: true,
            path: Some(path),
            notes_seq_repair_runs: AtomicUsize::new(0),
        };
        (dir, backend)
    }

    async fn issue_1029_create_entity_shaped_sequence(
        backend: &StorageBackend,
    ) -> Result<(), String> {
        let entities = backend
            .entities_for_namespace("tenant_ns")
            .map_err(|e| format!("entities_for_namespace: {e}"))?;
        let entity = khive_storage::entity::Entity::new("tenant_ns", "concept", "Issue1029Repro");
        let entity_id = entity.id;
        entities
            .upsert_entity(entity)
            .await
            .map_err(|e| format!("upsert_entity: {e}"))?;

        let text = backend.text("entities").map_err(|e| format!("text: {e}"))?;
        let doc = khive_storage::types::TextDocument {
            subject_id: entity_id,
            kind: khive_types::SubstrateKind::Entity,
            record_kind: None,
            title: Some("Issue1029Repro".to_string()),
            body: "issue 1029 repro body".to_string(),
            tags: vec![],
            namespace: "tenant_ns".to_string(),
            metadata: None,
            updated_at: chrono::Utc::now(),
        };
        text.upsert_document(doc)
            .await
            .map_err(|e| format!("fts_upsert: {e}"))
    }

    /// khive#1029 H1/H2 control: `KHIVE_WRITE_QUEUE` unset (legacy pool-mutex
    /// / standalone-connection path for both stores, sharing ONE
    /// `ConnectionPool` via ONE `StorageBackend` — the topology this test
    /// exists to confirm or kill as the lock source, isolated from any
    /// multi-pool or multi-backend wiring question).
    #[tokio::test]
    async fn issue_1029_create_entity_shaped_sequence_write_queue_off() {
        let (_dir, backend) = issue_1029_pool(false);
        let result = issue_1029_create_entity_shaped_sequence(&backend).await;
        assert!(
            result.is_ok(),
            "khive#1029 repro (KHIVE_WRITE_QUEUE off): fts_upsert step failed: {:?}",
            result.err()
        );
    }

    /// khive#1029 H1 direct test: `KHIVE_WRITE_QUEUE=1`, single shared
    /// `ConnectionPool`/`StorageBackend` (so the pool-wide `WriterTask` is
    /// shared by construction) — isolates whether the WriterTask's
    /// transaction lifecycle itself (not a multi-pool topology) is the lock
    /// source.
    #[tokio::test]
    async fn issue_1029_create_entity_shaped_sequence_write_queue_on() {
        let (_dir, backend) = issue_1029_pool(true);
        let result = issue_1029_create_entity_shaped_sequence(&backend).await;
        assert!(
            result.is_ok(),
            "khive#1029 repro (KHIVE_WRITE_QUEUE=1): fts_upsert step failed: {:?}",
            result.err()
        );
    }

    /// khive#1029 H2 direct test: TWO independent `ConnectionPool`s (hence
    /// two independent writer connections / two independent `WriterTask`
    /// `OnceLock`s) opened against the SAME tenant DB file — the shape a
    /// per-store (rather than per-backend) pool construction would produce.
    /// Entity writes go through pool A, the FTS write through pool B, each
    /// with `write_queue_enabled: Some(true)` so each independently spawns its own
    /// WriterTask on first access.
    #[tokio::test]
    async fn issue_1029_two_pools_same_file_write_queue_on() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("issue_1029_two_pools.db");

        let cfg = |p: std::path::PathBuf| crate::pool::PoolConfig {
            path: Some(p),
            busy_timeout: std::time::Duration::from_millis(200),
            write_queue_enabled: Some(true),
            ..crate::pool::PoolConfig::default()
        };

        let pool_a = ConnectionPool::new(cfg(path.clone())).expect("pool A should open");
        let backend_a = StorageBackend {
            pool: Arc::new(pool_a),
            is_file_backed: true,
            path: Some(path.clone()),
            notes_seq_repair_runs: AtomicUsize::new(0),
        };
        let pool_b = ConnectionPool::new(cfg(path.clone())).expect("pool B should open");
        let backend_b = StorageBackend {
            pool: Arc::new(pool_b),
            is_file_backed: true,
            path: Some(path),
            notes_seq_repair_runs: AtomicUsize::new(0),
        };

        let entities = backend_a
            .entities_for_namespace("tenant_ns")
            .expect("entities_for_namespace on pool A");
        let entity =
            khive_storage::entity::Entity::new("tenant_ns", "concept", "Issue1029TwoPools");
        let entity_id = entity.id;
        entities
            .upsert_entity(entity)
            .await
            .expect("pool A entity upsert should succeed");

        let text = backend_b.text("entities").expect("text on pool B");
        let doc = khive_storage::types::TextDocument {
            subject_id: entity_id,
            kind: khive_types::SubstrateKind::Entity,
            record_kind: None,
            title: Some("Issue1029TwoPools".to_string()),
            body: "issue 1029 two-pool repro body".to_string(),
            tags: vec![],
            namespace: "tenant_ns".to_string(),
            metadata: None,
            updated_at: chrono::Utc::now(),
        };
        let result = text.upsert_document(doc).await;
        assert!(
            result.is_ok(),
            "khive#1029 two-pool repro: fts_upsert on an independent pool for the \
             same tenant DB file failed: {:?}",
            result.err()
        );
    }

    /// Minimal thread-local capture subscriber for asserting emitted events —
    /// mirrors the capture subscriber in `checkpoint.rs`'s tick tests.
    struct StarvationCaptureSubscriber {
        events: Arc<std::sync::Mutex<Vec<std::collections::BTreeMap<String, String>>>>,
    }

    impl tracing::Subscriber for StarvationCaptureSubscriber {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            #[derive(Default)]
            struct FieldVisitor(std::collections::BTreeMap<String, String>);
            impl tracing::field::Visit for FieldVisitor {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    self.0
                        .insert(field.name().to_string(), format!("{value:?}"));
                }
            }
            let mut visitor = FieldVisitor::default();
            event.record(&mut visitor);
            self.events.lock().unwrap().push(visitor.0);
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// Regression coverage for the lock-starvation diagnostic itself: when a
    /// text write starves on the SQLite write lock, `with_writer_unmanaged`
    /// must emit the WARN carrying the `tx_registry` snapshot — operation
    /// name, open-transaction count, and the registered labels.
    ///
    /// `#[serial(tx_registry)]`: the registry is a process-wide singleton
    /// shared across this test binary; this group serializes every test that
    /// registers fixture entries or asserts snapshot contents (see
    /// `checkpoint.rs`, `pool.rs`, `sql_bridge.rs`). The assertion checks the
    /// fixture label is PRESENT rather than the snapshot being exactly one
    /// entry, so unrelated short-lived production registrations elsewhere in
    /// the binary cannot flake it.
    #[tokio::test]
    #[serial_test::serial(tx_registry)]
    async fn issue_1029_starvation_warn_reports_registered_transactions() {
        let (_dir, backend) = issue_1029_pool(false);
        // Create the store (and its FTS DDL) BEFORE the lock is held, so the
        // starvation happens inside `upsert_document` itself.
        let text = backend.text("entities").expect("text store");

        // Hold a genuine SQLite write lock on a separate standalone writer
        // connection, with a registered fixture transaction the diagnostic
        // must surface.
        let holder = backend
            .pool
            .open_standalone_writer()
            .expect("holder connection");
        holder
            .execute_batch("BEGIN IMMEDIATE")
            .expect("holder BEGIN IMMEDIATE");
        let fixture =
            khive_storage::tx_registry::register(Some("issue_1029_fixture_tx".to_string()));

        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = StarvationCaptureSubscriber {
            events: Arc::clone(&events),
        };
        let guard = tracing::subscriber::set_default(subscriber);

        let doc = khive_storage::types::TextDocument {
            subject_id: uuid::Uuid::new_v4(),
            kind: khive_types::SubstrateKind::Entity,
            record_kind: None,
            title: Some("Issue1029Starved".to_string()),
            body: "issue 1029 starvation diagnostic body".to_string(),
            tags: vec![],
            namespace: "tenant_ns".to_string(),
            metadata: None,
            updated_at: chrono::Utc::now(),
        };
        let result = text.upsert_document(doc).await;

        drop(guard);
        drop(fixture);
        holder
            .execute_batch("ROLLBACK")
            .expect("holder ROLLBACK releases the lock");

        assert!(
            result.is_err(),
            "upsert_document must starve while another connection holds the write lock"
        );

        let events = events.lock().unwrap();
        let warn = events
            .iter()
            .find(|fields| {
                fields
                    .get("message")
                    .is_some_and(|m| m.contains("text write starved"))
            })
            .unwrap_or_else(|| panic!("expected a starvation WARN, captured events: {events:?}"));
        assert!(
            warn.get("op").is_some_and(|op| op.contains("fts_upsert")),
            "WARN must name the starved operation, got: {warn:?}"
        );
        assert!(
            warn.get("open_txs")
                .is_some_and(|txs| txs.contains("issue_1029_fixture_tx")),
            "WARN must list the registered holder label, got: {warn:?}"
        );
        let count: usize = warn
            .get("open_tx_count")
            .expect("WARN must carry open_tx_count")
            .parse()
            .expect("open_tx_count must be numeric");
        assert!(
            count >= 1,
            "open_tx_count must count the fixture, got {count}"
        );
    }
}

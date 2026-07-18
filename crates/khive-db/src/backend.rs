//! Concrete storage backend providing capability traits.
//!
//! `StorageBackend` owns a `ConnectionPool` and provides factory methods for all
//! capability traits (`GraphStore`, `NoteStore`, `EventStore`, `VectorStore`,
//! `TextSearch`, `SqlAccess`). File-backed for production; in-memory for tests.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use rusqlite::OptionalExtension;

use crate::error::SqliteError;
use crate::pool::{ConnectionPool, PoolConfig};
use crate::sql_bridge::SqlBridge;
use crate::stores::{blob, entity, event, graph, note, sparse, text, vectors};

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
    /// Opens (or creates) the database at `path`. The underlying pool provides
    /// 1 writer + N readers in WAL mode for concurrent access.
    /// No schema is applied — call `apply_schema()` for each service.
    pub fn sqlite(path: impl AsRef<Path>) -> Result<Self, SqliteError> {
        crate::extension::ensure_extensions_loaded();
        let resolved = path.as_ref().to_path_buf();
        let config = PoolConfig {
            path: Some(resolved.clone()),
            ..PoolConfig::default()
        };
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
    /// pool; this PRAGMA extends that protection to the writer slot.
    ///
    /// The database file must already exist — unlike `sqlite()` this constructor
    /// does not create a new file.
    pub fn sqlite_read_only(path: impl AsRef<Path>) -> Result<Self, SqliteError> {
        crate::extension::ensure_extensions_loaded();
        let resolved = path.as_ref().to_path_buf();
        let config = PoolConfig {
            path: Some(resolved.clone()),
            read_only: true,
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
    /// Executes each DDL statement idempotently via `execute_batch`. Each
    /// statement MUST be self-contained and use `CREATE TABLE IF NOT EXISTS`
    /// (or equivalent idempotent DDL) so that calling this method more than
    /// once does not fail.
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
        for &stmt in statements {
            writer.conn().execute_batch(stmt)?;
        }
        Ok(())
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
        let writer = self.pool.try_writer()?;
        entity::ensure_entities_schema(writer.conn())?;

        Ok(Arc::new(entity::SqlEntityStore::new(
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
        let writer = self.pool.try_writer()?;
        graph::ensure_graph_schema(writer.conn())?;

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
        let writer = self.pool.try_writer()?;
        event::ensure_events_schema(writer.conn())?;

        Ok(Arc::new(event::SqlEventStore::new_scoped(
            Arc::clone(&self.pool),
            self.is_file_backed,
            namespace.trim().to_string(),
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
        let writer = self.pool.try_writer()?;

        // Detect old-schema vec0 tables that predate the `field` column.
        // vec0 virtual tables do not support ALTER TABLE, so we must drop and recreate
        // the table if it exists without the `field` column. Vector data is a cache —
        // callers can re-embed from the source record after the table is rebuilt.
        // Use pragma_table_info to check columns directly; substring matching on the
        // CREATE DDL is fragile (a model_key containing "field" would false-match).
        let table_exists: bool = writer
            .conn()
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
                rusqlite::params![&table],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(SqliteError::Rusqlite)?
            .is_some();

        if table_exists {
            // V17 migration (vector_embedding_model_tag_preserving_rebuild) adds
            // `field` and `embedding_model` to all pre-existing vec0 tables at
            // migration time.  If this table still lacks either column post-migration
            // that indicates the database was not migrated — return a hard error
            // rather than silently dropping data.
            let pragma = format!("PRAGMA table_xinfo({})", table);
            let mut stmt = writer.conn().prepare(&pragma)?;
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
                    "vec0 table '{}' is missing required column(s) (field={}, \
                     embedding_model={}); this is a pre-v0.2.8 vector schema and is \
                     not supported — recreate the database",
                    table, has_field, has_embedding_model,
                )));
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

        let writer = self.pool.try_writer()?;
        sparse::ensure_sparse_schema(writer.conn(), model_key).map_err(SqliteError::Rusqlite)?;

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
             tokenize = '{}'\
             )",
            table_key, tokenizer
        );
        let writer = self.pool.try_writer()?;
        writer.conn().execute_batch(&ddl)?;

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

    /// Is this a file-backed backend?
    pub fn is_file_backed(&self) -> bool {
        self.is_file_backed
    }

    /// Return the directory containing the backend's database file, or `None`
    /// for an in-memory backend.
    pub fn data_dir(&self) -> Option<std::path::PathBuf> {
        self.path.as_ref()?.parent().map(|p| p.to_path_buf())
    }

    /// Return the backend's full database file path, or `None` for an
    /// in-memory backend.
    pub fn db_path(&self) -> Option<&Path> {
        self.path.as_deref()
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

#[cfg(test)]
mod tests {
    use super::*;
    use khive_storage::types::{SqlStatement, SqlValue};

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

        let ro = StorageBackend::sqlite_read_only(&path).unwrap();
        let store = match ro.text("ro_test") {
            Ok(store) => store,
            Err(_) => return,
        };

        let doc = TextDocument {
            subject_id: uuid::Uuid::new_v4(),
            kind: SubstrateKind::Entity,
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
        assert_eq!(store.get(&content_ref).await.unwrap(), bytes);
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
            write_queue_enabled,
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
    /// with `write_queue_enabled: true` so each independently spawns its own
    /// WriterTask on first access.
    #[tokio::test]
    async fn issue_1029_two_pools_same_file_write_queue_on() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("issue_1029_two_pools.db");

        let cfg = |p: std::path::PathBuf| crate::pool::PoolConfig {
            path: Some(p),
            busy_timeout: std::time::Duration::from_millis(200),
            write_queue_enabled: true,
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

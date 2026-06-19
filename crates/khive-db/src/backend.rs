//! Concrete storage backend providing capability traits.
//!
//! `StorageBackend` owns a `ConnectionPool` and provides factory methods for all
//! capability traits (`GraphStore`, `NoteStore`, `EventStore`, `VectorStore`,
//! `TextSearch`, `SqlAccess`). File-backed for production; in-memory for tests.

use std::path::Path;
use std::sync::Arc;

use rusqlite::OptionalExtension;

use crate::error::SqliteError;
use crate::pool::{ConnectionPool, PoolConfig};
use crate::sql_bridge::SqlBridge;
use crate::stores::{entity, event, graph, note, sparse, text, vectors};

/// Concrete storage backend providing capability traits.
pub struct StorageBackend {
    pool: Arc<ConnectionPool>,
    is_file_backed: bool,
}

impl StorageBackend {
    /// File-backed SQLite database.
    ///
    /// Opens (or creates) the database at `path`. The underlying pool provides
    /// 1 writer + N readers in WAL mode for concurrent access.
    /// No schema is applied — call `apply_schema()` for each service.
    pub fn sqlite(path: impl AsRef<Path>) -> Result<Self, SqliteError> {
        crate::extension::ensure_extensions_loaded();
        let config = PoolConfig {
            path: Some(path.as_ref().to_path_buf()),
            ..PoolConfig::default()
        };
        let pool = ConnectionPool::new(config)?;
        Ok(Self {
            pool: Arc::new(pool),
            is_file_backed: true,
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
        let config = PoolConfig {
            path: Some(path.as_ref().to_path_buf()),
            ..PoolConfig::default()
        };
        let pool = ConnectionPool::new(config)?;
        // Set PRAGMA query_only = ON on the writer connection so that any write
        // attempt is rejected at the SQLite level regardless of which code path
        // reaches the writer.
        {
            let writer = pool.try_writer()?;
            writer
                .conn()
                .pragma_update(None, "query_only", "ON")
                .map_err(SqliteError::Rusqlite)?;
        }
        Ok(Self {
            pool: Arc::new(pool),
            is_file_backed: true,
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

        Ok(Arc::new(note::SqlNoteStore::new(
            Arc::clone(&self.pool),
            self.is_file_backed,
        )))
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

    /// Is this a file-backed backend?
    pub fn is_file_backed(&self) -> bool {
        self.is_file_backed
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
}

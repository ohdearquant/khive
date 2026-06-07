//! SQLite storage backend for the khive knowledge graph runtime.
//!
//! Provides entity, note, event, edge, FTS5 text search, and optional
//! `sqlite-vec` vector storage over a WAL-mode connection pool.

/// Concrete storage backend providing capability-trait factories.
pub mod backend;
/// Error types for the SQLite layer.
pub mod error;
/// SQLite extension registration (sqlite-vec auto-extension).
pub mod extension;
/// Schema migration system (versioned migrations).
pub mod migrations;
/// WAL-mode connection pool: one writer, N concurrent readers.
pub mod pool;
/// `SqlAccess` trait bridge to `ConnectionPool`.
pub mod sql_bridge;
/// Per-substrate store implementations (entity, note, graph, event, text, vectors, sparse).
pub mod stores;

pub use backend::StorageBackend;
pub use error::SqliteError;
pub use migrations::{
    query_embedding_models, run_migrations, EmbeddingModelRegistryRecord, Migration,
    ServiceSchemaPlan, VersionedMigration, MIGRATIONS,
};
pub use pool::{ConnectionPool, PoolConfig, ReaderGuard, WriterGuard};
pub use sql_bridge::SqlBridge;

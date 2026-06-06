//! SQLite storage backend for the khive knowledge graph runtime.
//!
//! Implements the capability traits from `khive-storage` (entities, notes, edges,
//! events, vectors, FTS5, sparse) backed by a WAL-mode SQLite database with
//! connection pooling.

pub mod backend;
pub mod error;
pub mod extension;
pub mod migrations;
pub mod pool;
pub mod sql_bridge;
pub mod stores;

pub use backend::StorageBackend;
pub use error::SqliteError;
pub use migrations::{
    query_embedding_models, run_migrations, EmbeddingModelRegistryRecord, Migration,
    ServiceSchemaPlan, VersionedMigration, MIGRATIONS,
};
pub use pool::{ConnectionPool, PoolConfig, ReaderGuard, WriterGuard};
pub use sql_bridge::SqlBridge;

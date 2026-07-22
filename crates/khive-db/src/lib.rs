//! SQLite storage backend for the khive knowledge graph runtime.
//!
//! Provides entity, note, event, edge, FTS5 text search, and optional
//! `sqlite-vec` vector storage over a WAL-mode connection pool.

/// Concrete storage backend providing capability-trait factories.
pub mod backend;
/// Periodic WAL checkpoint task.
pub mod checkpoint;
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
/// Cross-process WAL-pin attribution sidecar (ADR-091 Amendment 2 Plank B).
/// The sidecar write path (heartbeat/beacon) and identity primitives are
/// portable; directory enumeration (`enumerate_live`) is Unix-only — its
/// only caller is the daemon's checkpoint task, and daemon mode itself
/// requires Unix (see `khive-mcp/src/serve.rs`).
pub mod walpin;
/// Single-writer task and bounded write queue (ADR-067 Component A).
pub mod writer_task;

pub use backend::StorageBackend;
pub use checkpoint::{
    checkpoint_once, run_checkpoint_task, CheckpointConfig, CheckpointLifecycleOwner,
    CheckpointTick,
};
pub use checkpoint::{run_session_sweep_task, SessionSweepConfig, SweepBackend};
pub use error::SqliteError;
pub use migrations::{
    inspect_schema_version, query_embedding_models, read_schema_version, run_migrations,
    EmbeddingModelRegistryRecord, Migration, ServiceSchemaPlan, VersionedMigration, MIGRATIONS,
};
pub use pool::{ConnectionPool, PoolConfig, ReaderGuard, WriterGuard};
pub use sql_bridge::SqlBridge;
pub use writer_task::WriterTaskHandle;

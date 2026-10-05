//! SQLite storage backend for the khive knowledge graph runtime.
//!
//! Provides entity, note, event, edge, FTS5 text search, and optional
//! `sqlite-vec` vector storage over a WAL-mode connection pool.

/// Concrete storage backend providing capability-trait factories.
pub mod backend;
/// Periodic WAL checkpoint task.
pub mod checkpoint;
// Kept internal while the code-map constructor and SQLite callback wiring land.
#[allow(dead_code)]
mod code_map_vfs;
/// Durable database owner identity paired with the opened physical file.
pub mod database_owner_identity;
/// Read-only-by-intent database-integrity and WAL/checkpoint diagnostics.
pub mod diagnostics;
/// Error types for the SQLite layer.
pub mod error;
/// SQLite extension registration (sqlite-vec auto-extension).
pub mod extension;
/// Physical file identity shared by pool admission and backend alias routing.
#[cfg(any(unix, windows))]
pub mod file_identity;
mod fts_maintenance;
/// Schema migration system (versioned migrations).
pub mod migrations;
/// What a live store's schema says about `namespace` (ADR-189).
pub mod namespace_census;
/// Moving records between namespaces (ADR-189).
pub mod namespace_move;
/// A store fixture reproducing the namespace split, for the move's own arms.
#[cfg(any(test, feature = "test-support"))]
pub mod namespace_move_fixture;
/// Feature-gated namespace-bounded FTS5 trigram prototype.
#[cfg(feature = "namespace-trigram-proto")]
pub mod namespace_trigram_proto;
/// WAL-mode connection pool: one writer, N concurrent readers.
pub mod pool;
mod read_cancellation;
/// `SqlAccess` trait bridge to `ConnectionPool`.
pub mod sql_bridge;
#[cfg(any(test, feature = "test-support"))]
mod statement_observer;
/// Per-substrate store implementations (entity, note, graph, event, text, vectors, sparse).
pub mod stores;
/// Append-only NDJSON writer-timeout event sink (crate-internal).
mod timeout_sink;
/// Cross-process WAL-pin attribution sidecar (ADR-091 Amendment 2 Plank B).
/// The sidecar write path (heartbeat/beacon) and identity primitives are
/// portable; directory collection (`enumerate_live`/`housekeep_live`) is
/// Unix-only — its only caller is the daemon's checkpoint task, and daemon
/// mode itself requires Unix (see `khive-mcp/src/serve.rs`).
pub mod walpin;
/// Single-writer task and bounded write queue (ADR-067 Component A).
pub mod writer_task;

#[cfg(test)]
mod writer_busy_fixture;

#[cfg(test)]
mod test_process;

pub use backend::StorageBackend;
pub use checkpoint::{
    checkpoint_once, run_checkpoint_task, CheckpointConfig, CheckpointLifecycleOwner,
    CheckpointTick,
};
pub use checkpoint::{run_session_sweep_task, SessionSweepConfig, SweepBackend};
pub use database_owner_identity::{DatabaseOwnerIdentity, DatabaseOwnerIdentityError};
pub use error::{
    SqliteError, SQLITE_WAL_CAPACITY_REFUSED_STAGE, SQLITE_WAL_CAPACITY_UNAVAILABLE_STAGE,
};
pub use fts_maintenance::{
    fts_maintenance_counters, FtsIndexStructure, FtsLevelStructure, FtsMaintenanceCounters,
    FtsSegmentDiagnostics,
};
pub use khive_storage::{
    await_request_read_phase, effective_request_read_deadline, ensure_request_read_active,
    inherit_request_read_cancellation, inherit_request_read_context, request_read_is_cancelled,
    request_read_timeout_from_env, scope_request_read_cancellation, scope_request_read_deadline,
    scope_request_read_deadline_at, wait_for_request_read_cancellation, RequestReadDeadline,
    DEFAULT_REQUEST_READ_TIMEOUT_SECS,
};
pub use migrations::{
    inspect_schema_is_current, inspect_schema_version, query_embedding_models, read_schema_version,
    run_migrations, EmbeddingModelRegistryRecord, Migration, ServiceSchemaPlan, VersionedMigration,
    MIGRATIONS,
};
pub use pool::{
    CheckpointGuard, CheckpointResult, ConnectionPool, PoolConfig, ReaderGuard, ReaderRow,
    WalCeilingPolicy, WalCeilingSource, WriterGuard,
};
#[cfg(any(test, feature = "test-support"))]
pub use read_cancellation::scope_test_read_progress;
pub use read_cancellation::{sqlite_interrupt_grace_from_env, DEFAULT_SQLITE_INTERRUPT_GRACE_MS};
pub use sql_bridge::SqlBridge;
#[cfg(any(test, feature = "test-support"))]
pub use statement_observer::{StartedStatement, StatementStartObservation};
pub use writer_task::WriterTaskHandle;

#[cfg(test)]
mod namespace_move_fixture_tests;
#[cfg(test)]
mod stream_schema_tests;

#[cfg(test)]
mod reader_lease_tests;

#[cfg(test)]
mod raw_sql_reader_admission_tests;

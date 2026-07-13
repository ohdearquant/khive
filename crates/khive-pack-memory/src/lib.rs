//! Memory pack — `memory.remember` and `memory.recall` verbs with decay-aware ranking.

pub(crate) mod ann;
pub mod config;
pub mod handlers;
mod pack;
pub(crate) mod query_cache;
pub mod recall_feedback;
pub mod rerank;
pub mod scoring;
#[doc(hidden)]
pub mod text_gather;
pub mod tunable;

pub use pack::MemoryPack;

/// Bump the durable memory-ANN corpus epoch (#812). `kkernel reindex` mutates
/// note vectors and deletes the persisted
/// Vamana snapshot directly, out of process from any running khive daemon —
/// its in-memory write-generation counter (`ann::bump_generation`) is simply
/// unreachable from a separate process. Call this after invalidating the
/// snapshot so a warm daemon sharing the same database file has a durable
/// signal to observe on its next amortized freshness check
/// (`ann::maybe_check_durable_epoch`, sampled from the recall path) and
/// schedules a rebuild instead of serving pre-reindex vectors indefinitely.
pub async fn bump_memory_ann_epoch(
    rt: &khive_runtime::KhiveRuntime,
) -> Result<u64, khive_runtime::RuntimeError> {
    ann::bump_durable_epoch(rt).await
}

/// Ensure the `memory_ann_epoch` table exists on `rt` (idempotent, #812).
/// Declared via
/// `MemoryPack::SCHEMA_PLAN` for daemon boot (`server.rs`/`serve.rs` apply
/// every loaded pack's schema plan up front), but `kkernel reindex` runs
/// directly against a raw `KhiveRuntime` without ever booting a pack
/// registry — it calls this explicitly, once, before its first
/// `bump_memory_ann_epoch`, so a reindex against a brand-new database (no
/// daemon ever started) doesn't fail before this table exists.
pub async fn ensure_ann_epoch_schema(
    rt: &khive_runtime::KhiveRuntime,
) -> Result<(), khive_runtime::RuntimeError> {
    ann::ensure_epoch_schema(rt).await
}

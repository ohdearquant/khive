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

/// Bump the durable memory-ANN corpus epoch (#812 review REQUEST CHANGES
/// HIGH). `kkernel reindex` mutates note vectors and deletes the persisted
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

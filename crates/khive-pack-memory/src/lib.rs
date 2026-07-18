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

#[cfg(test)]
mod test_support;

pub use pack::MemoryPack;

/// Increment and return the durable ANN corpus epoch after snapshot invalidation.
///
/// Returns a runtime error when the database update fails. Warm daemons observe the
/// epoch asynchronously and schedule a rebuild. See `crates/khive-pack-memory/docs/api/ann-lifecycle.md`.
pub async fn bump_memory_ann_epoch(
    rt: &khive_runtime::KhiveRuntime,
) -> Result<u64, khive_runtime::RuntimeError> {
    ann::bump_durable_epoch(rt).await
}

/// Create the durable ANN epoch table if it does not exist.
///
/// The operation is idempotent and returns a runtime error on schema failure. Raw-runtime
/// reindex callers must invoke it before their first epoch bump. See `crates/khive-pack-memory/docs/api/ann-lifecycle.md`.
pub async fn ensure_ann_epoch_schema(
    rt: &khive_runtime::KhiveRuntime,
) -> Result<(), khive_runtime::RuntimeError> {
    ann::ensure_epoch_schema(rt).await
}

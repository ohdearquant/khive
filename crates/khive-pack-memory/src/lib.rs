//! Memory pack — `memory.remember` and `memory.recall` verbs with decay-aware ranking.
//!
//! Registers the `memory` note kind, depends on the `kg` pack, and routes
//! recall through FTS + vector fusion with configurable decay and reranking.

pub(crate) mod ann;
pub mod config;
pub mod handlers;
mod pack;
pub(crate) mod query_cache;
pub mod rerank;
pub mod scoring;
#[doc(hidden)]
pub mod text_gather;
pub mod tunable;

pub use pack::MemoryPack;

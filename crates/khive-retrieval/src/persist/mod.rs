//! Retrieval index persistence using SQLite.
//!
//! Write-through snapshots for HNSW and BM25 indexes; rebuild from snapshot on cold start.
//! Requires the `persist` feature flag.

mod bm25;
mod core;
mod hnsw;
mod shadow;

// INLINE TEST JUSTIFICATION: tests require access to private `setup_test_persistence()` and
// internal `RetrievalPersistence` constructor; moving them to crates/tests/ would require
// making those items pub(crate) and restructuring the internal schema helpers.
#[cfg(test)]
mod tests;

pub use core::{PersistError, PersistenceStats, RetrievalPersistence};
pub use shadow::{ShadowMetrics, ShadowValidationConfig, ShadowValidationResult};

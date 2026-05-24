//! Checkpoint protocol for fold-based index persistence.
//!
//! Provides generic snapshot envelopes and in-memory storage for use
//! by HNSW and other fold-managed indexes.
//!
//! # Formal proof reference
//!
//! `proofs/Retrieval/HNSW.lean` — checkpoint correctness guarantees
//! used in HNSW snapshot/restore cycles.
//!
//! # Architecture
//!
//! ```text
//! HnswIndex ──snapshot──> HnswSnapshot ──wrap──> Checkpoint<HnswSnapshot>
//!                                                       │
//!                                         CheckpointStore::save(...)
//! ```
//!
//! The snapshot types and this checkpoint envelope are always available;
//! the fold feature flag in consuming crates gates whether they are exposed
//! to callers.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use khive_types::Hash32;

use crate::context::FoldContext;
use crate::error::FoldError;

/// Generic checkpoint envelope wrapping an arbitrary fold state snapshot.
///
/// Carries metadata (ID, timestamp, hash, fold version) alongside the
/// serializable state so consumers can verify and load the correct snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint<S> {
    /// Human-readable checkpoint identifier (e.g. `"hnsw_idx:ckpt-1"`).
    pub id: String,

    /// The snapshot state captured at this checkpoint.
    pub state: S,

    /// Unique identifier for this checkpoint instance.
    pub uuid: Uuid,

    /// Content hash of the state for integrity verification.
    pub hash: Hash32,

    /// Number of entries processed when this checkpoint was taken.
    pub entries_processed: usize,

    /// Fold context at checkpoint time.
    pub context: FoldContext,

    /// Monotonically increasing fold schema version.
    pub fold_version: usize,

    /// Wall-clock time when this checkpoint was created.
    pub created_at: DateTime<Utc>,
}

impl<S> Checkpoint<S> {
    /// Create a new checkpoint.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<String>,
        state: S,
        uuid: Uuid,
        hash: Hash32,
        entries_processed: usize,
        context: FoldContext,
        fold_version: usize,
    ) -> Self {
        Self {
            id: id.into(),
            state,
            uuid,
            hash,
            entries_processed,
            context,
            fold_version,
            created_at: Utc::now(),
        }
    }
}

/// Trait for checkpoint persistence backends.
///
/// The key is the checkpoint `id` string. `load_latest` returns the
/// checkpoint whose prefix matches — defined as all checkpoints whose
/// `id` starts with the given prefix, selecting the most recently created.
pub trait CheckpointStore<S> {
    /// Persist a checkpoint.
    fn save(&self, checkpoint: &Checkpoint<S>) -> Result<(), FoldError>
    where
        S: Clone;

    /// Load a checkpoint by its exact `id`.
    fn load(&self, id: &str) -> Result<Option<Checkpoint<S>>, FoldError>
    where
        S: Clone;

    /// Load the most recently created checkpoint whose `id` starts with `prefix`.
    ///
    /// Returns `None` when no checkpoints match the prefix.
    fn load_latest(&self, prefix: &str) -> Result<Option<Checkpoint<S>>, FoldError>
    where
        S: Clone;
}

/// In-memory checkpoint store backed by a `RwLock<HashMap>`.
///
/// Suitable for tests and single-process usage where durability is not
/// required. Production deployments should implement [`CheckpointStore`]
/// with durable storage (e.g. SQLite via `khive-db`).
pub struct InMemoryCheckpointStore<S> {
    inner: Arc<RwLock<HashMap<String, Checkpoint<S>>>>,
}

impl<S> InMemoryCheckpointStore<S> {
    /// Create a new empty in-memory store.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl<S> Default for InMemoryCheckpointStore<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: Clone + Send + Sync + 'static> CheckpointStore<S> for InMemoryCheckpointStore<S> {
    fn save(&self, checkpoint: &Checkpoint<S>) -> Result<(), FoldError>
    where
        S: Clone,
    {
        let mut guard = self
            .inner
            .write()
            .map_err(|e| FoldError::LockPoisoned(e.to_string()))?;
        guard.insert(checkpoint.id.clone(), checkpoint.clone());
        Ok(())
    }

    fn load(&self, id: &str) -> Result<Option<Checkpoint<S>>, FoldError>
    where
        S: Clone,
    {
        let guard = self
            .inner
            .read()
            .map_err(|e| FoldError::LockPoisoned(e.to_string()))?;
        Ok(guard.get(id).cloned())
    }

    fn load_latest(&self, prefix: &str) -> Result<Option<Checkpoint<S>>, FoldError>
    where
        S: Clone,
    {
        let guard = self
            .inner
            .read()
            .map_err(|e| FoldError::LockPoisoned(e.to_string()))?;

        let latest = guard
            .values()
            .filter(|c| c.id.starts_with(prefix))
            .max_by_key(|c| c.created_at);

        Ok(latest.cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_checkpoint(id: &str, entries: usize) -> Checkpoint<String> {
        Checkpoint::new(
            id,
            format!("state-{entries}"),
            Uuid::new_v4(),
            Hash32::ZERO,
            entries,
            FoldContext::new(),
            1,
        )
    }

    #[test]
    fn save_and_load_roundtrip() {
        let store: InMemoryCheckpointStore<String> = InMemoryCheckpointStore::new();
        let ckpt = sample_checkpoint("my-index:ckpt-1", 100);
        store.save(&ckpt).unwrap();
        let loaded = store.load("my-index:ckpt-1").unwrap().unwrap();
        assert_eq!(loaded.state, "state-100");
        assert_eq!(loaded.entries_processed, 100);
    }

    #[test]
    fn load_missing_returns_none() {
        let store: InMemoryCheckpointStore<String> = InMemoryCheckpointStore::new();
        assert!(store.load("nonexistent").unwrap().is_none());
    }

    #[test]
    fn load_latest_returns_most_recent() {
        let store: InMemoryCheckpointStore<String> = InMemoryCheckpointStore::new();

        let ckpt1 = sample_checkpoint("idx:ckpt-1", 10);
        store.save(&ckpt1).unwrap();
        // small sleep so created_at differs
        std::thread::sleep(std::time::Duration::from_millis(5));
        let ckpt2 = sample_checkpoint("idx:ckpt-2", 20);
        store.save(&ckpt2).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let ckpt3 = sample_checkpoint("idx:ckpt-3", 30);
        store.save(&ckpt3).unwrap();

        let latest = store.load_latest("idx").unwrap().unwrap();
        assert_eq!(latest.entries_processed, 30);
    }

    #[test]
    fn load_latest_no_match_returns_none() {
        let store: InMemoryCheckpointStore<String> = InMemoryCheckpointStore::new();
        store.save(&sample_checkpoint("other:ckpt-1", 5)).unwrap();
        assert!(store.load_latest("my-index").unwrap().is_none());
    }

    #[test]
    fn load_latest_prefix_isolation() {
        let store: InMemoryCheckpointStore<String> = InMemoryCheckpointStore::new();
        store.save(&sample_checkpoint("alpha:ckpt-1", 10)).unwrap();
        store.save(&sample_checkpoint("beta:ckpt-1", 999)).unwrap();

        let latest_alpha = store.load_latest("alpha").unwrap().unwrap();
        assert_eq!(latest_alpha.entries_processed, 10);
    }

    #[test]
    fn checkpoint_fields_accessible() {
        let ckpt: Checkpoint<u32> = Checkpoint::new(
            "test:ckpt",
            42u32,
            Uuid::new_v4(),
            Hash32::ZERO,
            7,
            FoldContext::new(),
            3,
        );
        assert_eq!(ckpt.state, 42);
        assert_eq!(ckpt.entries_processed, 7);
        assert_eq!(ckpt.fold_version, 3);
    }
}

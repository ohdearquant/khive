//! Generic checkpoint envelope and in-memory store for fold-managed indexes.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};
#[cfg(feature = "serde")]
use serde::Deserialize;
use serde::Serialize;
use uuid::Uuid;

use khive_types::Hash32;

use crate::context::FoldContext;
use crate::error::FoldError;

/// Generic checkpoint envelope wrapping an arbitrary fold state snapshot.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Checkpoint<S> {
    /// Human-readable checkpoint identifier (e.g. `"hnsw_idx:ckpt-1"`).
    pub id: String,

    /// The snapshot state captured at this checkpoint.
    pub state: S,

    /// Unique identifier for this checkpoint instance.
    pub uuid: Uuid,

    /// BLAKE3 content hash of the state; verified on load.
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

impl<S: Serialize> Checkpoint<S> {
    /// Create a new checkpoint, computing the BLAKE3 hash of the state.
    // REASON: Checkpoint::new requires id, state, uuid, entries_processed, context, and
    // fold_version — each is a semantically distinct field with no natural grouping into
    // a builder or sub-struct without breaking the public API.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<String>,
        state: S,
        uuid: Uuid,
        entries_processed: usize,
        context: FoldContext,
        fold_version: usize,
    ) -> Result<Self, FoldError> {
        let bytes = serde_json::to_vec(&state)?;
        let hash = Hash32::from_blake3(&bytes);
        Ok(Self {
            id: id.into(),
            state,
            uuid,
            hash,
            entries_processed,
            context,
            fold_version,
            // Foundation layer does not call Utc::now() — epoch is the safe default.
            // Callers that need the current time should set created_at after construction.
            created_at: DateTime::<Utc>::default(),
        })
    }

    /// Create a checkpoint with a pre-computed hash (for deserialization / testing).
    // REASON: with_hash mirrors the new() parameter set (minus auto-computed hash) for
    // deserialization and testing; same structural constraint as new() above.
    #[allow(clippy::too_many_arguments)]
    pub fn with_hash(
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
            // Foundation layer does not call Utc::now() — epoch is the safe default.
            created_at: DateTime::<Utc>::default(),
        }
    }
}

/// Trait for checkpoint persistence backends.
pub trait CheckpointStore<S> {
    /// Persist a checkpoint, computing and storing an integrity hash.
    fn save(&self, checkpoint: Checkpoint<S>) -> Result<(), FoldError>
    where
        S: Clone + Serialize;

    /// Load a checkpoint by exact `id`, verifying the integrity hash.
    fn load(&self, id: &str) -> Result<Option<Checkpoint<S>>, FoldError>
    where
        S: Clone + Serialize;

    /// Load the most recently created checkpoint whose `id` starts with `prefix`.
    ///
    /// Verify the selected checkpoint's integrity hash as in [`Self::load`].
    /// A verification or serialization error is returned, not replaced by an
    /// older matching checkpoint. Shared state may still mutate after the check.
    fn load_latest(&self, prefix: &str) -> Result<Option<Checkpoint<S>>, FoldError>
    where
        S: Clone + Serialize;

    /// Delete the checkpoint with the given `id`.
    fn delete(&self, id: &str) -> Result<(), FoldError>;

    /// List all checkpoint `id` strings currently stored.
    fn list(&self) -> Result<Vec<String>, FoldError>;
}

/// In-memory checkpoint store backed by a `RwLock<HashMap>`.
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

/// Verify the selected clone without changing selection or falling back.
fn verify_checkpoint<S: Serialize>(checkpoint: Checkpoint<S>) -> Result<Checkpoint<S>, FoldError> {
    let bytes = serde_json::to_vec(&checkpoint.state)?;
    let computed = Hash32::from_blake3(&bytes);
    if !checkpoint.hash.eq_ct(&computed) {
        return Err(FoldError::IntegrityMismatch {
            id: checkpoint.id.clone(),
            stored: checkpoint.hash.to_string(),
            computed: computed.to_string(),
        });
    }
    Ok(checkpoint)
}

impl<S: Clone + Send + Sync + Serialize + 'static> CheckpointStore<S>
    for InMemoryCheckpointStore<S>
{
    fn save(&self, checkpoint: Checkpoint<S>) -> Result<(), FoldError>
    where
        S: Clone + Serialize,
    {
        // Recompute the hash from the state to ensure the stored hash is canonical.
        let bytes = serde_json::to_vec(&checkpoint.state)?;
        let computed = Hash32::from_blake3(&bytes);
        let mut stored = checkpoint;
        stored.hash = computed;

        let mut guard = self
            .inner
            .write()
            .map_err(|e| FoldError::LockPoisoned(e.to_string()))?;
        guard.insert(stored.id.clone(), stored);
        Ok(())
    }

    fn load(&self, id: &str) -> Result<Option<Checkpoint<S>>, FoldError>
    where
        S: Clone + Serialize,
    {
        let guard = self
            .inner
            .read()
            .map_err(|e| FoldError::LockPoisoned(e.to_string()))?;
        let Some(checkpoint) = guard.get(id).cloned() else {
            return Ok(None);
        };

        Ok(Some(verify_checkpoint(checkpoint)?))
    }

    fn load_latest(&self, prefix: &str) -> Result<Option<Checkpoint<S>>, FoldError>
    where
        S: Clone + Serialize,
    {
        let guard = self
            .inner
            .read()
            .map_err(|e| FoldError::LockPoisoned(e.to_string()))?;

        let latest = guard
            .values()
            .filter(|c| c.id.starts_with(prefix))
            // Tiebreak on uuid for determinism when created_at is equal.
            .max_by_key(|c| (c.created_at, c.uuid));

        latest.cloned().map(verify_checkpoint).transpose()
    }

    fn delete(&self, id: &str) -> Result<(), FoldError> {
        let mut guard = self
            .inner
            .write()
            .map_err(|e| FoldError::LockPoisoned(e.to_string()))?;
        if guard.remove(id).is_none() {
            return Err(FoldError::CheckpointNotFound(id.to_owned()));
        }
        Ok(())
    }

    fn list(&self) -> Result<Vec<String>, FoldError> {
        let guard = self
            .inner
            .read()
            .map_err(|e| FoldError::LockPoisoned(e.to_string()))?;
        let keys: Vec<String> = guard.keys().cloned().collect();
        Ok(sort_checkpoint_keys(keys))
    }
}

/// Sort a `Vec<String>` of checkpoint IDs into lexicographic order.
/// See crates/khive-fold/docs/api/checkpoint.md#ordering for why this is
/// a standalone helper.
pub fn sort_checkpoint_keys(mut keys: Vec<String>) -> Vec<String> {
    keys.sort();
    keys
}

#[cfg(test)]
#[path = "checkpoint_tests.rs"]
mod tests;

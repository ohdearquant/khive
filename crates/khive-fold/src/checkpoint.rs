//! Generic checkpoint envelope and in-memory store for fold-managed indexes.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
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

        // Verify integrity: recompute hash from state and compare.
        let bytes = serde_json::to_vec(&checkpoint.state)?;
        let computed = Hash32::from_blake3(&bytes);
        if !checkpoint.hash.eq_ct(&computed) {
            return Err(FoldError::IntegrityMismatch {
                id: id.to_owned(),
                stored: checkpoint.hash.to_string(),
                computed: computed.to_string(),
            });
        }

        Ok(Some(checkpoint))
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

        Ok(latest.cloned())
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
        // Sort so callers get a stable, deterministic order regardless of HashMap seed.
        let mut keys: Vec<String> = guard.keys().cloned().collect();
        keys.sort();
        Ok(keys)
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
            entries,
            FoldContext::new(),
            1,
        )
        .expect("sample_checkpoint should not fail serialization")
    }

    #[test]
    fn save_and_load_roundtrip() {
        let store: InMemoryCheckpointStore<String> = InMemoryCheckpointStore::new();
        let ckpt = sample_checkpoint("my-index:ckpt-1", 100);
        store.save(ckpt).unwrap();
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
        use chrono::Duration;

        let store: InMemoryCheckpointStore<String> = InMemoryCheckpointStore::new();
        let base = DateTime::<Utc>::default();

        // Build checkpoints with explicit, strictly ordered created_at values
        // so load_latest is deterministic without relying on wall-clock time.
        let mut ckpt1 = sample_checkpoint("idx:ckpt-1", 10);
        ckpt1.created_at = base;
        let mut ckpt2 = sample_checkpoint("idx:ckpt-2", 20);
        ckpt2.created_at = base + Duration::milliseconds(5);
        let mut ckpt3 = sample_checkpoint("idx:ckpt-3", 30);
        ckpt3.created_at = base + Duration::milliseconds(10);

        store.save(ckpt1).unwrap();
        store.save(ckpt2).unwrap();
        store.save(ckpt3).unwrap();

        let latest = store.load_latest("idx").unwrap().unwrap();
        assert_eq!(latest.entries_processed, 30);
    }

    #[test]
    fn load_latest_no_match_returns_none() {
        let store: InMemoryCheckpointStore<String> = InMemoryCheckpointStore::new();
        store.save(sample_checkpoint("other:ckpt-1", 5)).unwrap();
        assert!(store.load_latest("my-index").unwrap().is_none());
    }

    #[test]
    fn load_latest_prefix_isolation() {
        let store: InMemoryCheckpointStore<String> = InMemoryCheckpointStore::new();
        store.save(sample_checkpoint("alpha:ckpt-1", 10)).unwrap();
        store.save(sample_checkpoint("beta:ckpt-1", 999)).unwrap();

        let latest_alpha = store.load_latest("alpha").unwrap().unwrap();
        assert_eq!(latest_alpha.entries_processed, 10);
    }

    #[test]
    fn checkpoint_fields_accessible() {
        let ckpt: Checkpoint<u32> =
            Checkpoint::new("test:ckpt", 42u32, Uuid::new_v4(), 7, FoldContext::new(), 3).unwrap();
        assert_eq!(ckpt.state, 42);
        assert_eq!(ckpt.entries_processed, 7);
        assert_eq!(ckpt.fold_version, 3);
    }

    // --- Additional tests (F-NEW-8) ---

    #[cfg(feature = "serde")]
    #[test]
    fn serde_roundtrip() {
        let ckpt = sample_checkpoint("serde:test", 42);
        let json = serde_json::to_string(&ckpt).expect("serialize");
        let restored: Checkpoint<String> = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(ckpt.id, restored.id);
        assert_eq!(ckpt.state, restored.state);
        assert_eq!(ckpt.entries_processed, restored.entries_processed);
        assert_eq!(ckpt.fold_version, restored.fold_version);
        assert_eq!(ckpt.uuid, restored.uuid);
        // Hash bytes should survive the roundtrip unchanged.
        assert_eq!(ckpt.hash.as_bytes(), restored.hash.as_bytes());
    }

    #[test]
    fn delete_existing_succeeds() {
        let store: InMemoryCheckpointStore<String> = InMemoryCheckpointStore::new();
        store.save(sample_checkpoint("del:ckpt-1", 1)).unwrap();
        store.delete("del:ckpt-1").unwrap();
        assert!(store.load("del:ckpt-1").unwrap().is_none());
    }

    #[test]
    fn delete_nonexistent_returns_not_found() {
        let store: InMemoryCheckpointStore<String> = InMemoryCheckpointStore::new();
        let err = store.delete("nope").unwrap_err();
        assert!(
            matches!(err, FoldError::CheckpointNotFound(ref id) if id == "nope"),
            "expected CheckpointNotFound, got {err:?}"
        );
    }

    #[test]
    fn list_returns_all_ids() {
        let store: InMemoryCheckpointStore<String> = InMemoryCheckpointStore::new();
        store.save(sample_checkpoint("a:ckpt-1", 1)).unwrap();
        store.save(sample_checkpoint("b:ckpt-1", 2)).unwrap();
        store.save(sample_checkpoint("c:ckpt-1", 3)).unwrap();
        let mut ids = store.list().unwrap();
        ids.sort();
        assert_eq!(ids, vec!["a:ckpt-1", "b:ckpt-1", "c:ckpt-1"]);
    }

    #[test]
    fn list_empty_store() {
        let store: InMemoryCheckpointStore<String> = InMemoryCheckpointStore::new();
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn save_overwrite_replaces_previous() {
        let store: InMemoryCheckpointStore<String> = InMemoryCheckpointStore::new();
        let ckpt1 = sample_checkpoint("overwrite:ckpt-1", 10);
        store.save(ckpt1).unwrap();

        // Save again with the same id but different state.
        let ckpt2 = Checkpoint::new(
            "overwrite:ckpt-1",
            "new-state".to_string(),
            Uuid::new_v4(),
            99,
            FoldContext::new(),
            2,
        )
        .unwrap();
        store.save(ckpt2).unwrap();

        let loaded = store.load("overwrite:ckpt-1").unwrap().unwrap();
        assert_eq!(loaded.state, "new-state");
        assert_eq!(loaded.entries_processed, 99);
        // Only one entry with that id.
        let ids = store.list().unwrap();
        assert_eq!(ids.iter().filter(|id| *id == "overwrite:ckpt-1").count(), 1);
    }

    #[test]
    fn integrity_mismatch_on_corrupted_hash() {
        let store: InMemoryCheckpointStore<String> = InMemoryCheckpointStore::new();
        let ckpt = sample_checkpoint("integrity:ckpt-1", 5);
        store.save(ckpt).unwrap();

        // Directly corrupt the stored hash by replacing it with ZERO.
        {
            let mut guard = store.inner.write().unwrap();
            if let Some(c) = guard.get_mut("integrity:ckpt-1") {
                c.hash = Hash32::ZERO;
            }
        }

        let err = store.load("integrity:ckpt-1").unwrap_err();
        assert!(
            matches!(err, FoldError::IntegrityMismatch { .. }),
            "expected IntegrityMismatch, got {err:?}"
        );
    }

    #[test]
    fn concurrent_saves_all_land() {
        use std::sync::Arc;
        use std::thread;

        let store = Arc::new(InMemoryCheckpointStore::<String>::new());
        let n = 20usize;
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let s = Arc::clone(&store);
                thread::spawn(move || {
                    s.save(sample_checkpoint(&format!("concurrent:ckpt-{i}"), i))
                        .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().expect("thread panicked");
        }
        let ids = store.list().unwrap();
        assert_eq!(ids.len(), n, "expected {n} checkpoints, got {}", ids.len());
    }

    /// list() must return keys in lexicographic order regardless of HashMap
    /// insertion order or HashMap seed across processes.
    #[test]
    fn list_is_sorted() {
        let store: InMemoryCheckpointStore<String> = InMemoryCheckpointStore::new();
        // Insert in non-alphabetical order.
        store.save(sample_checkpoint("z:ckpt-1", 1)).unwrap();
        store.save(sample_checkpoint("a:ckpt-1", 2)).unwrap();
        store.save(sample_checkpoint("m:ckpt-1", 3)).unwrap();
        let ids = store.list().unwrap();
        let mut expected = ids.clone();
        expected.sort();
        assert_eq!(ids, expected, "list() must return sorted keys");
    }
}

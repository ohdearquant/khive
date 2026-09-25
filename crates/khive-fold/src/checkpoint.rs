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
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    // Clone keeps the handles shared, exposing post-save changes through the
    // public state API without changing the store's private map or saved hash.
    #[derive(Clone, Debug)]
    struct MutableCheckpointState {
        value: Arc<AtomicU64>,
        fail_serialization: Arc<AtomicBool>,
        serializations: Arc<AtomicU64>,
    }

    impl MutableCheckpointState {
        fn new(value: u64) -> Self {
            Self {
                value: Arc::new(AtomicU64::new(value)),
                fail_serialization: Arc::new(AtomicBool::new(false)),
                serializations: Arc::new(AtomicU64::new(0)),
            }
        }
    }

    impl Serialize for MutableCheckpointState {
        fn serialize<T>(&self, serializer: T) -> Result<T::Ok, T::Error>
        where
            T: serde::Serializer,
        {
            self.serializations.fetch_add(1, Ordering::SeqCst);
            if self.fail_serialization.load(Ordering::SeqCst) {
                return Err(serde::ser::Error::custom(
                    "test checkpoint serialization disabled",
                ));
            }
            serializer.serialize_u64(self.value.load(Ordering::SeqCst))
        }
    }

    fn mutable_checkpoint(
        id: &str,
        state: &MutableCheckpointState,
        uuid: u128,
    ) -> Checkpoint<MutableCheckpointState> {
        Checkpoint::new(
            id,
            state.clone(),
            Uuid::from_u128(uuid),
            1,
            FoldContext::new(),
            1,
        )
        .unwrap()
    }

    fn mismatch_fields(error: FoldError) -> (String, String, String) {
        match error {
            FoldError::IntegrityMismatch {
                id,
                stored,
                computed,
            } => (id, stored, computed),
            other => panic!("expected IntegrityMismatch, got {other:?}"),
        }
    }

    #[test]
    fn load_latest_rejects_shared_state_change() {
        let store = InMemoryCheckpointStore::new();
        let state = MutableCheckpointState::new(7);
        store
            .save(mutable_checkpoint("shared:one", &state, 1))
            .unwrap();
        assert_eq!(store.load("shared:one").unwrap().unwrap().id, "shared:one");
        assert_eq!(
            store.load_latest("shared:").unwrap().unwrap().id,
            "shared:one"
        );

        state.value.store(8, Ordering::SeqCst);
        assert!(store.load_latest("absent:").unwrap().is_none());
        let exact = mismatch_fields(store.load("shared:one").unwrap_err());
        // RED on the base: latest returns Ok(Some(_)) without verifying.
        let latest = mismatch_fields(store.load_latest("shared:").unwrap_err());
        assert_eq!(latest, exact);
        assert_eq!(latest.0, "shared:one");
        assert_ne!(latest.1, latest.2);

        state.value.store(7, Ordering::SeqCst);
        assert_eq!(
            store.load_latest("shared:").unwrap().unwrap().id,
            "shared:one"
        );
    }

    #[test]
    fn load_latest_rejects_corrupt_uuid_winner_without_fallback() {
        let store = InMemoryCheckpointStore::new();
        let older = MutableCheckpointState::new(10);
        let winner = MutableCheckpointState::new(20);
        let unrelated = MutableCheckpointState::new(30);
        // Same epoch timestamps; UUID, not insertion order or sleep, breaks ties.
        store
            .save(mutable_checkpoint("tie:winner", &winner, 2))
            .unwrap();
        store
            .save(mutable_checkpoint("tie:older", &older, 1))
            .unwrap();
        let mut other = mutable_checkpoint("other:one", &unrelated, 3);
        other.created_at += chrono::Duration::seconds(1);
        store.save(other).unwrap();
        unrelated.fail_serialization.store(true, Ordering::SeqCst);
        assert_eq!(store.load_latest("tie:").unwrap().unwrap().id, "tie:winner");

        winner.value.store(21, Ordering::SeqCst);
        let old_count = older.serializations.load(Ordering::SeqCst);
        let winner_count = winner.serializations.load(Ordering::SeqCst);
        let unrelated_count = unrelated.serializations.load(Ordering::SeqCst);
        // RED on the base: a corrupt winner is returned, not an error.
        let error = store.load_latest("tie:").unwrap_err();
        let (id, stored, computed) = mismatch_fields(error);
        assert_eq!(id, "tie:winner");
        assert_ne!(stored, computed);
        assert_eq!(
            winner.serializations.load(Ordering::SeqCst),
            winner_count + 1
        );
        assert_eq!(older.serializations.load(Ordering::SeqCst), old_count);
        assert_eq!(
            unrelated.serializations.load(Ordering::SeqCst),
            unrelated_count
        );
        assert_eq!(store.load("tie:older").unwrap().unwrap().id, "tie:older");
    }

    #[test]
    fn load_latest_propagates_selected_state_serialization_error() {
        let store = InMemoryCheckpointStore::new();
        let state = MutableCheckpointState::new(42);
        store
            .save(mutable_checkpoint("serialize:one", &state, 1))
            .unwrap();
        state.fail_serialization.store(true, Ordering::SeqCst);
        let exact_error = store.load("serialize:one").unwrap_err();
        assert!(matches!(exact_error, FoldError::Serialization(_)));
        let before = state.serializations.load(Ordering::SeqCst);
        // RED on the base: latest does not call the serializer at all.
        let latest_error = store.load_latest("serialize:").unwrap_err();
        assert!(matches!(latest_error, FoldError::Serialization(_)));
        assert_eq!(latest_error.to_string(), exact_error.to_string());
        assert_eq!(state.serializations.load(Ordering::SeqCst), before + 1);
        state.fail_serialization.store(false, Ordering::SeqCst);
        assert_eq!(
            store.load_latest("serialize:").unwrap().unwrap().id,
            "serialize:one"
        );
    }

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

    // Reverse-sorted input is the worst case for an unsorted implementation;
    // see design.md#test-rationale-notes.
    #[test]
    fn sort_checkpoint_keys_produces_lexicographic_order() {
        // Intentionally REVERSE alphabetical — worst case for unsorted implementations.
        let unsorted = vec![
            "z:ckpt-3".to_string(),
            "m:ckpt-2".to_string(),
            "a:ckpt-1".to_string(),
        ];
        let sorted = sort_checkpoint_keys(unsorted);
        assert_eq!(
            sorted,
            vec!["a:ckpt-1", "m:ckpt-2", "z:ckpt-3"],
            "sort_checkpoint_keys must produce lexicographic order; got {sorted:?}"
        );
    }

    /// Integration: `InMemoryCheckpointStore::list` must return keys in
    /// lexicographic order regardless of insertion order.
    #[test]
    fn list_is_sorted() {
        let store: InMemoryCheckpointStore<String> = InMemoryCheckpointStore::new();
        // Insert in non-alphabetical order.
        store.save(sample_checkpoint("z:ckpt-1", 1)).unwrap();
        store.save(sample_checkpoint("a:ckpt-1", 2)).unwrap();
        store.save(sample_checkpoint("m:ckpt-1", 3)).unwrap();
        let ids = store.list().unwrap();
        assert_eq!(
            ids,
            vec!["a:ckpt-1", "m:ckpt-1", "z:ckpt-1"],
            "list() must return sorted keys; got {ids:?}"
        );
    }
}

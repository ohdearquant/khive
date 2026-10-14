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

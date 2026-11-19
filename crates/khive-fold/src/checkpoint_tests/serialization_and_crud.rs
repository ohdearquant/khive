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

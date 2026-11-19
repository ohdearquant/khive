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

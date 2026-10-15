use super::*;
use crate::HnswIndex;

fn make_index(dims: usize, count: usize) -> HnswIndex {
    let mut index = HnswIndex::new(dims);
    for i in 0..count {
        let id = NodeId::new([(i & 0xFF) as u8; 16]);
        let vec = vec![i as f32; dims];
        index.insert(id, vec).unwrap();
    }
    index
}

#[test]
fn test_register_and_create_alias() {
    let mgr = IndexAliasManager::new(Duration::from_secs(1));

    let index = make_index(4, 5);
    mgr.register_collection("v1", index).unwrap();
    mgr.create_alias("active", "v1").unwrap();

    assert_eq!(mgr.collection_count(), 1);
    assert_eq!(mgr.alias_count(), 1);
    assert_eq!(mgr.resolve_alias("active"), Some("v1".to_string()));
}

#[test]
fn test_register_duplicate_collection() {
    let mgr = IndexAliasManager::new(Duration::from_secs(1));
    mgr.register_collection("v1", make_index(4, 1)).unwrap();

    let result = mgr.register_collection("v1", make_index(4, 1));
    assert!(matches!(
        result,
        Err(AliasError::CollectionAlreadyExists(_))
    ));
}

#[test]
fn test_create_alias_missing_collection() {
    let mgr = IndexAliasManager::new(Duration::from_secs(1));
    let result = mgr.create_alias("active", "nonexistent");
    assert!(matches!(result, Err(AliasError::CollectionNotFound(_))));
}

#[test]
fn test_create_duplicate_alias() {
    let mgr = IndexAliasManager::new(Duration::from_secs(1));
    mgr.register_collection("v1", make_index(4, 1)).unwrap();
    mgr.create_alias("active", "v1").unwrap();

    let result = mgr.create_alias("active", "v1");
    assert!(matches!(result, Err(AliasError::AliasAlreadyExists(_))));
}

#[test]
fn test_acquire_reader() {
    let mgr = IndexAliasManager::new(Duration::from_secs(1));
    mgr.register_collection("v1", make_index(4, 5)).unwrap();
    mgr.create_alias("active", "v1").unwrap();

    let guard = mgr.acquire_reader("active").unwrap();
    assert_eq!(guard.len(), 5);
    assert_eq!(mgr.reader_count("v1"), Some(1));

    drop(guard);
    assert_eq!(mgr.reader_count("v1"), Some(0));
}

#[test]
fn test_acquire_reader_missing_alias() {
    let mgr = IndexAliasManager::new(Duration::from_secs(1));
    let result = mgr.acquire_reader("nonexistent");
    assert!(matches!(result, Err(AliasError::AliasNotFound(_))));
}

#[test]
fn test_switch_alias() {
    let mgr = IndexAliasManager::new(Duration::from_secs(1));
    mgr.register_collection("v1", make_index(4, 5)).unwrap();
    mgr.register_collection("v2", make_index(4, 10)).unwrap();
    mgr.create_alias("active", "v1").unwrap();

    let old = mgr.switch_alias("active", "v2", None).unwrap();
    assert_eq!(old, "v1");
    assert_eq!(mgr.resolve_alias("active"), Some("v2".to_string()));

    // Reader should now get v2
    let guard = mgr.acquire_reader("active").unwrap();
    assert_eq!(guard.len(), 10);
}

#[test]
fn test_switch_alias_with_failing_validator() {
    let mgr = IndexAliasManager::new(Duration::from_secs(1));
    mgr.register_collection("v1", make_index(4, 5)).unwrap();
    mgr.register_collection("v2", make_index(4, 10)).unwrap();
    mgr.create_alias("active", "v1").unwrap();

    // Validator that always fails
    struct FailValidator;
    impl IndexValidator for FailValidator {
        fn validate(&self, _: &HnswIndex) -> Result<(), AliasError> {
            Err(AliasError::ValidationFailed {
                reason: "test failure".to_string(),
                recall: Some(0.5),
                min_recall: Some(0.95),
            })
        }
    }

    let result = mgr.switch_alias("active", "v2", Some(&FailValidator));
    assert!(matches!(result, Err(AliasError::ValidationFailed { .. })));

    // Alias should still point to v1
    assert_eq!(mgr.resolve_alias("active"), Some("v1".to_string()));
}

/// Regression for #1438: a validated index cannot be retired and replaced
/// under the same name before the alias is published.
#[tokio::test]
async fn test_switch_alias_rejects_replacement_after_validation() {
    struct PassValidator;

    impl IndexValidator for PassValidator {
        fn validate(&self, _index: &HnswIndex) -> Result<(), AliasError> {
            Ok(())
        }
    }

    let mgr = Arc::new(IndexAliasManager::new(Duration::from_secs(1)));
    mgr.register_collection("v1", make_index(4, 5)).unwrap();
    mgr.register_collection("v2", make_index(4, 10)).unwrap();
    mgr.create_alias("active", "v1").unwrap();

    let (validated_tx, validated_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let switch_manager = Arc::clone(&mgr);
    let switch = std::thread::spawn(move || {
        switch_manager.switch_alias_with_test_hook("active", "v2", Some(&PassValidator), || {
            validated_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        })
    });

    validated_rx.recv().unwrap();
    mgr.drain_and_remove("v2").await.unwrap();
    mgr.register_collection("v2", make_index(4, 12)).unwrap();
    release_tx.send(()).unwrap();

    assert!(matches!(
        switch.join().unwrap(),
        Err(AliasError::CollectionNotFound(name)) if name == "v2"
    ));
    assert_eq!(mgr.resolve_alias("active"), Some("v1".to_string()));
    assert_eq!(mgr.acquire_reader("active").unwrap().len(), 5);

    mgr.create_alias("replacement", "v2").unwrap();
    assert_eq!(mgr.acquire_reader("replacement").unwrap().len(), 12);
}

#[tokio::test]
async fn test_drain_and_remove() {
    let mgr = IndexAliasManager::new(Duration::from_secs(1));
    mgr.register_collection("v1", make_index(4, 5)).unwrap();

    // No readers -- drain should succeed immediately
    mgr.drain_and_remove("v1").await.unwrap();
    assert_eq!(mgr.collection_count(), 0);
}

/// Regression for #1438: cleanup of a known candidate must not remove a
/// different index that later reuses the same collection name.
#[tokio::test]
async fn test_retire_collection_preserves_same_name_replacement() {
    let mgr = IndexAliasManager::new(Duration::from_secs(1));
    mgr.register_collection("v1", make_index(4, 5)).unwrap();
    let original = Arc::clone(&mgr.collections.read().get("v1").unwrap().index);

    mgr.drain_and_remove("v1").await.unwrap();
    mgr.register_collection("v1", make_index(4, 10)).unwrap();

    assert!(matches!(
        mgr.retire_collection("v1", Some(&original)),
        Err(AliasError::CollectionNotFound(name)) if name == "v1"
    ));
    mgr.create_alias("active", "v1").unwrap();
    assert_eq!(mgr.acquire_reader("active").unwrap().len(), 10);
}

/// Regression for #1438: rejecting retirement must leave every alias and
/// collection usable, including aliases unrelated to the rejected target.
#[tokio::test]
async fn test_drain_and_remove_rejects_aliased_collection_without_state_change() {
    let mgr = IndexAliasManager::new(Duration::from_secs(1));
    mgr.register_collection("v1", make_index(4, 5)).unwrap();
    mgr.register_collection("v2", make_index(4, 10)).unwrap();
    mgr.create_alias("active", "v1").unwrap();
    mgr.create_alias("mirror", "v1").unwrap();
    mgr.create_alias("fallback", "v2").unwrap();

    let result = mgr.drain_and_remove("v1").await;
    assert!(matches!(
        result,
        Err(AliasError::CollectionInUse(name)) if name == "v1"
    ));

    assert_eq!(mgr.collection_count(), 2);
    assert_eq!(mgr.alias_count(), 3);
    assert_eq!(mgr.resolve_alias("active"), Some("v1".to_string()));
    assert_eq!(mgr.resolve_alias("mirror"), Some("v1".to_string()));
    assert_eq!(mgr.resolve_alias("fallback"), Some("v2".to_string()));
    assert_eq!(mgr.acquire_reader("active").unwrap().len(), 5);
    assert_eq!(mgr.acquire_reader("mirror").unwrap().len(), 5);
    assert_eq!(mgr.acquire_reader("fallback").unwrap().len(), 10);
}

#[tokio::test]
async fn test_concurrent_read_during_swap() {
    let mgr = Arc::new(IndexAliasManager::new(Duration::from_secs(5)));
    mgr.register_collection("v1", make_index(4, 5)).unwrap();
    mgr.register_collection("v2", make_index(4, 10)).unwrap();
    mgr.create_alias("active", "v1").unwrap();

    // Acquire a reader on v1
    let guard = mgr.acquire_reader("active").unwrap();
    assert_eq!(guard.len(), 5);

    // Swap alias to v2 while v1 reader is active
    let old = mgr.switch_alias("active", "v2", None).unwrap();
    assert_eq!(old, "v1");

    // Old reader should still see v1 (5 vectors)
    assert_eq!(guard.len(), 5);

    // New reader should see v2 (10 vectors)
    let new_guard = mgr.acquire_reader("active").unwrap();
    assert_eq!(new_guard.len(), 10);

    // Drop the old reader
    drop(guard);

    // Now drain should succeed for v1
    let mgr_clone = Arc::clone(&mgr);
    mgr_clone.drain_and_remove("v1").await.unwrap();

    // v1 should be gone, v2 should remain
    assert_eq!(mgr.collection_count(), 1);
}

#[tokio::test]
async fn test_migrate() {
    let mgr = Arc::new(IndexAliasManager::new(Duration::from_secs(5)));
    mgr.register_collection("v1", make_index(4, 5)).unwrap();
    mgr.create_alias("active", "v1").unwrap();

    // Prepare vectors for the new index
    let vectors: Vec<(NodeId, Vec<f32>)> = (0..8u8)
        .map(|i| (NodeId::new([i; 16]), vec![i as f32; 4]))
        .collect();

    let config = HnswConfig::with_dimensions(4);
    let report = mgr.migrate("active", vectors, config, None).await.unwrap();

    assert_eq!(report.old_size, 5);
    assert_eq!(report.new_size, 8);

    // The alias should now point to the new collection
    let guard = mgr.acquire_reader("active").unwrap();
    assert_eq!(guard.len(), 8);
    assert_eq!(mgr.reader_count("v1"), None);
    assert_eq!(mgr.collection_count(), 1);
}

/// Regression for #1437: replacing a candidate immediately after it is
/// registered must not make the migration validate, publish, or report the
/// replacement under the reused name.
#[tokio::test]
async fn test_migrate_rejects_candidate_replacement_after_registration() {
    use std::sync::atomic::AtomicUsize;

    struct RecordingPassValidator {
        validated_size: Arc<AtomicUsize>,
    }

    impl IndexValidator for RecordingPassValidator {
        fn validate(&self, index: &HnswIndex) -> Result<(), AliasError> {
            self.validated_size
                .store(index.len_live(), Ordering::SeqCst);
            Ok(())
        }
    }

    let manager = Arc::new(IndexAliasManager::new(Duration::from_secs(5)));
    manager.register_collection("v1", make_index(4, 5)).unwrap();
    manager.create_alias("active", "v1").unwrap();

    let validated_size = Arc::new(AtomicUsize::new(0));
    let hook_manager = Arc::clone(&manager);
    let result = manager
        .migrate_inner(
            "active",
            (0..8u8)
                .map(|i| (NodeId::new([i; 16]), vec![i as f32; 4]))
                .collect(),
            HnswConfig::with_dimensions(4),
            Some(Box::new(RecordingPassValidator {
                validated_size: Arc::clone(&validated_size),
            })),
            move |candidate| {
                let retired = hook_manager
                    .retire_collection(candidate, None)
                    .expect("fresh migration candidate should be unreferenced");
                drop(retired);
                hook_manager
                    .register_collection(candidate, make_index(4, 12))
                    .expect("same-name replacement should register");
            },
        )
        .await;

    let replacement_name = match result {
        Err(AliasError::CollectionNotFound(name)) => name,
        other => panic!("replacement identity must prevent publication, got {other:?}"),
    };
    assert!(replacement_name.starts_with("v1_migrated_"));
    assert_eq!(
        validated_size.load(Ordering::SeqCst),
        8,
        "validation must use the originally built candidate, not its same-name replacement"
    );
    assert_eq!(manager.resolve_alias("active"), Some("v1".to_string()));
    assert_eq!(manager.acquire_reader("active").unwrap().len(), 5);

    manager
        .create_alias("replacement", &replacement_name)
        .expect("failed migration cleanup must preserve the same-name replacement");
    assert_eq!(manager.acquire_reader("replacement").unwrap().len(), 12);
    assert_eq!(manager.collection_count(), 2);
}

/// Regression for #1437: two migrations that both captured `v1` may
/// build and validate concurrently, but exactly one may publish and
/// retire `v1`. The loser must return an ownership conflict and remove
/// its unreferenced candidate.
#[test]
fn test_concurrent_migrations_compare_and_switch_and_cleanup_loser() {
    use std::sync::{mpsc, Mutex};

    struct BlockingPassValidator {
        ready: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl IndexValidator for BlockingPassValidator {
        fn validate(&self, _index: &HnswIndex) -> Result<(), AliasError> {
            self.ready.send(()).unwrap();
            // If test setup fails and drops the sender, unblock the worker
            // so it cannot remain detached from a panicking test.
            let _ = self.release.lock().unwrap().recv();
            Ok(())
        }
    }

    let manager = Arc::new(IndexAliasManager::new(Duration::from_secs(5)));
    manager.register_collection("v1", make_index(4, 5)).unwrap();
    manager.create_alias("active", "v1").unwrap();

    let (ready_tx, ready_rx) = mpsc::channel();
    let (first_release_tx, first_release_rx) = mpsc::channel();
    let (second_release_tx, second_release_rx) = mpsc::channel();

    let spawn_migration = |vector_count: u8, release: mpsc::Receiver<()>| {
        let manager = Arc::clone(&manager);
        let ready = ready_tx.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let vectors = (0..vector_count)
                .map(|i| (NodeId::new([i; 16]), vec![i as f32; 4]))
                .collect();
            runtime.block_on(manager.migrate(
                "active",
                vectors,
                HnswConfig::with_dimensions(4),
                Some(Box::new(BlockingPassValidator {
                    ready,
                    release: Mutex::new(release),
                })),
            ))
        })
    };

    let first = spawn_migration(8, first_release_rx);
    let second = spawn_migration(12, second_release_rx);

    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("first migration did not reach validation");
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("second migration did not reach validation");

    let candidates_before_switch: Vec<String> = manager
        .collections
        .read()
        .keys()
        .filter(|name| name.starts_with("v1_migrated_"))
        .cloned()
        .collect();
    assert_eq!(
        candidates_before_switch.len(),
        2,
        "both migrations must own distinct registered candidates before publication"
    );

    first_release_tx.send(()).unwrap();
    second_release_tx.send(()).unwrap();
    let first_result = first.join().unwrap();
    let second_result = second.join().unwrap();

    let (winner, loser) = match (&first_result, &second_result) {
        (Ok(winner), Err(loser)) | (Err(loser), Ok(winner)) => (winner, loser),
        outcomes => panic!("expected one migration winner and one loser, got {outcomes:?}"),
    };
    assert!(matches!(
        loser,
        AliasError::AliasTargetChanged {
            alias,
            expected,
            actual,
        } if alias == "active"
            && expected == "v1"
            && actual == &winner.new_collection
    ));
    assert_eq!(
        manager.resolve_alias("active"),
        Some(winner.new_collection.clone()),
        "the successful report must name the final alias target"
    );
    assert_eq!(manager.reader_count("v1"), None);
    assert_eq!(
        manager.collection_count(),
        1,
        "the loser candidate and original collection must both be retired"
    );
    for candidate in candidates_before_switch {
        assert_eq!(
            manager.reader_count(&candidate).is_some(),
            candidate == winner.new_collection.as_str(),
            "only the winning replacement may remain registered"
        );
    }
}

/// Regression for #1438: migrating one alias must keep an old collection
/// registered when another alias still targets it.
#[tokio::test]
async fn test_migrate_preserves_old_collection_used_by_another_alias() {
    let mgr = Arc::new(IndexAliasManager::new(Duration::from_secs(5)));
    mgr.register_collection("v1", make_index(4, 5)).unwrap();
    mgr.create_alias("active", "v1").unwrap();
    mgr.create_alias("fallback", "v1").unwrap();

    let vectors: Vec<(NodeId, Vec<f32>)> = (0..8u8)
        .map(|i| (NodeId::new([i; 16]), vec![i as f32; 4]))
        .collect();
    let report = mgr
        .migrate("active", vectors, HnswConfig::with_dimensions(4), None)
        .await
        .unwrap();

    assert_eq!(
        mgr.resolve_alias("active"),
        Some(report.new_collection.clone())
    );
    assert_eq!(mgr.resolve_alias("fallback"), Some("v1".to_string()));
    assert_eq!(mgr.acquire_reader("active").unwrap().len(), 8);
    assert_eq!(mgr.acquire_reader("fallback").unwrap().len(), 5);
    assert_eq!(mgr.collection_count(), 2);
}

/// Regression for #1438: validation cleanup must retain a failed candidate
/// once an alias begins referencing it.
#[test]
fn test_migrate_validation_failure_preserves_aliased_candidate() {
    use std::sync::{mpsc, Mutex};

    struct BlockingFailValidator {
        entered: mpsc::SyncSender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl IndexValidator for BlockingFailValidator {
        fn validate(&self, _index: &HnswIndex) -> Result<(), AliasError> {
            self.entered.send(()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
            Err(AliasError::ValidationFailed {
                reason: "candidate rejected".to_string(),
                recall: Some(0.4),
                min_recall: Some(0.9),
            })
        }
    }

    let mgr = Arc::new(IndexAliasManager::new(Duration::from_secs(5)));
    mgr.register_collection("v1", make_index(4, 5)).unwrap();
    mgr.create_alias("active", "v1").unwrap();

    let (entered_tx, entered_rx) = mpsc::sync_channel(0);
    let (release_tx, release_rx) = mpsc::channel();
    let migration_manager = Arc::clone(&mgr);
    let migration = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let vectors = (0..8u8)
            .map(|i| (NodeId::new([i; 16]), vec![i as f32; 4]))
            .collect();
        runtime.block_on(migration_manager.migrate(
            "active",
            vectors,
            HnswConfig::with_dimensions(4),
            Some(Box::new(BlockingFailValidator {
                entered: entered_tx,
                release: Mutex::new(release_rx),
            })),
        ))
    });

    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("validator did not start");
    let candidate = mgr
        .collections
        .read()
        .keys()
        .find(|name| name.starts_with("v1_migrated_"))
        .cloned()
        .expect("registered migration candidate");
    let alias_result = mgr.create_alias("candidate", &candidate);
    release_tx.send(()).unwrap();
    let migration_result = migration.join().unwrap();

    alias_result.unwrap();
    assert!(matches!(
        migration_result,
        Err(AliasError::ValidationFailed {
            reason,
            recall: Some(0.4),
            min_recall: Some(0.9),
        }) if reason == "recall below threshold"
    ));
    assert_eq!(mgr.resolve_alias("active"), Some("v1".to_string()));
    assert_eq!(mgr.resolve_alias("candidate"), Some(candidate));
    assert_eq!(mgr.acquire_reader("active").unwrap().len(), 5);
    assert_eq!(mgr.acquire_reader("candidate").unwrap().len(), 8);
    assert_eq!(mgr.collection_count(), 2);
}

/// Regression for #1439: validation must retain only the selected index
/// snapshot, not a collection registry read lock.
#[test]
fn test_migrate_validation_releases_collection_lock() {
    use std::sync::{mpsc, Mutex};

    struct BlockingFailValidator {
        entered: mpsc::SyncSender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl IndexValidator for BlockingFailValidator {
        fn validate(&self, _index: &HnswIndex) -> Result<(), AliasError> {
            self.entered.send(()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
            Err(AliasError::ValidationFailed {
                reason: "blocked validator failure".to_string(),
                recall: Some(0.42),
                min_recall: Some(0.95),
            })
        }
    }

    let manager = Arc::new(IndexAliasManager::new(Duration::from_secs(5)));
    manager
        .register_collection("v1", make_index(4, 10))
        .unwrap();
    manager.create_alias("production", "v1").unwrap();

    let (entered_tx, entered_rx) = mpsc::sync_channel(0);
    let (release_tx, release_rx) = mpsc::channel();
    let migration_manager = Arc::clone(&manager);
    let migration = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let vectors = (0..20u8)
            .map(|i| (NodeId::new([i; 16]), vec![i as f32; 4]))
            .collect();
        runtime.block_on(migration_manager.migrate(
            "production",
            vectors,
            HnswConfig::with_dimensions(4),
            Some(Box::new(BlockingFailValidator {
                entered: entered_tx,
                release: Mutex::new(release_rx),
            })),
        ))
    });

    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("validator did not start");

    let registration_manager = Arc::clone(&manager);
    let (registration_tx, registration_rx) = mpsc::channel();
    let registration = std::thread::spawn(move || {
        let result = registration_manager.register_collection("unrelated", make_index(4, 1));
        registration_tx.send(result).unwrap();
    });

    let registration_result = registration_rx.recv_timeout(Duration::from_secs(5));
    release_tx.send(()).unwrap();
    let migration_result = migration.join().unwrap();
    registration.join().unwrap();

    registration_result
        .expect("collection registration blocked while validation was paused")
        .unwrap();
    match migration_result {
        Err(AliasError::ValidationFailed {
            reason,
            recall,
            min_recall,
        }) => {
            assert_eq!(reason, "recall below threshold");
            assert_eq!(recall, Some(0.42));
            assert_eq!(min_recall, Some(0.95));
        }
        other => panic!("unexpected migration result: {other:?}"),
    }
    assert_eq!(manager.resolve_alias("production"), Some("v1".to_string()));

    let collections = manager.collections.read();
    assert_eq!(collections.len(), 2);
    assert!(collections.contains_key("v1"));
    assert!(collections.contains_key("unrelated"));
    assert!(!collections
        .keys()
        .any(|name| name.starts_with("v1_migrated_")));
}

/// Regression for #417: acquiring a reader must not race a concurrent
/// alias switch + drain/remove of the collection the reader resolved
/// to. Pauses right after alias resolution (deterministic via channel,
/// no sleep), then performs a switch+drain+remove that would, on the
/// buggy code, remove the collection before the reader is counted.
#[tokio::test]
async fn test_acquire_reader_switch_drain_race_does_not_return_collection_not_found() {
    let mgr = Arc::new(IndexAliasManager::new(Duration::from_secs(5)));
    mgr.register_collection("v1", make_index(4, 5)).unwrap();
    mgr.register_collection("v2", make_index(4, 10)).unwrap();
    mgr.create_alias("active", "v1").unwrap();

    let (paused_tx, paused_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let (b_done_tx, b_done_rx) = std::sync::mpsc::channel::<()>();

    let mgr_a = Arc::clone(&mgr);
    let handle_a = std::thread::spawn(move || {
        mgr_a.acquire_reader_with_test_hook("active", || {
            paused_tx.send(()).expect("send paused signal");
            release_rx.recv().expect("recv release signal");
        })
    });

    // Wait until thread A has resolved alias -> v1 and is paused.
    paused_rx.recv().expect("recv paused signal");

    // Concurrently switch the alias to v2 and drain+remove v1 on a
    // second thread -- exactly what a migration does. On the buggy
    // code this has no lock contention and completes immediately,
    // removing v1 before A is released below. On the fixed code it
    // blocks behind A's held `aliases` read lock until A is released.
    let mgr_b = Arc::clone(&mgr);
    let handle_b = std::thread::spawn(move || {
        mgr_b.switch_alias("active", "v2", None).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build nested runtime for thread B");
        rt.block_on(mgr_b.drain_and_remove("v1")).unwrap();
        b_done_tx.send(()).ok();
    });

    // Give B a bounded window to run to completion if nothing blocks it
    // (the pre-fix case: B finishes almost instantly). Whether or not
    // it finished, release A next -- on the fixed code B is still
    // blocked at this point and only proceeds once A releases the
    // `aliases` lock below.
    let _ = b_done_rx.recv_timeout(Duration::from_millis(200));
    release_tx.send(()).expect("send release signal");

    let result = handle_a.join().expect("thread A join");
    let a_len = match result {
        Ok(guard) => {
            let len = guard.len();
            // Drop the guard so B's drain_and_remove (waiting on this
            // reader on the fixed code) does not block on join below.
            drop(guard);
            len
        }
        Err(e) => {
            panic!("acquire_reader must not return an error due to a switch/drain race, got {e:?}")
        }
    };
    handle_b.join().expect("thread B join");

    assert_eq!(a_len, 5, "must return the old v1 snapshot");
}

/// Regression for #418: if drain times out during migration, the old
/// collection must be retired from `self.collections` immediately
/// (not kept forever), even though the held reader guard keeps the
/// underlying index alive until it is dropped.
#[tokio::test]
async fn test_migrate_timeout_retires_old_collection_from_manager() {
    let mgr = Arc::new(IndexAliasManager::new(Duration::ZERO));
    mgr.register_collection("v1", make_index(4, 5)).unwrap();
    mgr.create_alias("active", "v1").unwrap();

    // Hold a reader guard on v1 so drain can never complete within the
    // zero-duration timeout.
    let guard = mgr.acquire_reader("active").unwrap();
    assert_eq!(guard.len(), 5);

    let vectors: Vec<(NodeId, Vec<f32>)> = (0..8u8)
        .map(|i| (NodeId::new([i; 16]), vec![i as f32; 4]))
        .collect();
    let config = HnswConfig::with_dimensions(4);

    let report = mgr
        .migrate("active", vectors, config, None)
        .await
        .expect("migration must succeed even if drain times out");
    assert_eq!(report.old_collection, "v1");

    assert_eq!(
        mgr.resolve_alias("active"),
        Some(report.new_collection.clone())
    );
    assert_eq!(
        mgr.reader_count("v1"),
        None,
        "old collection must no longer be manager-owned even though drain timed out"
    );
    assert_eq!(
        mgr.collection_count(),
        1,
        "manager must not keep the retired collection around forever"
    );

    // The held guard must still be usable until dropped.
    assert_eq!(guard.len(), 5);
    drop(guard);
    assert_eq!(mgr.collection_count(), 1);
}

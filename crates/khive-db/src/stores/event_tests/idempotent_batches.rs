#[tokio::test]
async fn idempotent_batch_uses_one_writer_acquisition() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("idempotent_acquisition.db");
    let pool_cfg = PoolConfig {
        path: Some(path.clone()),
        write_queue_enabled: Some(true),
        ..PoolConfig::for_test()
    };
    let pool = Arc::new(ConnectionPool::new(pool_cfg).unwrap());
    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(EVENTS_DDL).unwrap();
    }
    let store = SqlEventStore::new_scoped(Arc::clone(&pool), true, "default");

    let before = pool.writer_acquisition_snapshot().writer_task_acquisitions;

    let events: Vec<Event> = (0..3).map(|_| make_event("default")).collect();
    let ids: Vec<Uuid> = events.iter().map(|e| e.id).collect();
    let result = store.append_events_idempotent(events).await.unwrap();

    assert_eq!(result.rows.len(), 3);
    assert!(result
        .rows
        .iter()
        .all(|d| *d == EventAppendDisposition::Inserted));
    for id in ids {
        assert!(store.get_event(id).await.unwrap().is_some());
    }

    let after = pool.writer_acquisition_snapshot().writer_task_acquisitions;
    assert_eq!(
        after - before,
        1,
        "a 3-row idempotent batch must acquire the writer exactly once, not once per row"
    );
}

#[tokio::test]
async fn idempotent_retry_accepts_only_full_equality() {
    let store = setup_memory_store();

    let event = make_event("default");
    let id = event.id;

    let first = store
        .append_events_idempotent(vec![event.clone()])
        .await
        .unwrap();
    assert_eq!(first.rows, vec![EventAppendDisposition::Inserted]);

    // Retrying the identical event reproduces the same row exactly.
    let retry = store
        .append_events_idempotent(vec![event.clone()])
        .await
        .unwrap();
    assert_eq!(
        retry.rows,
        vec![EventAppendDisposition::AlreadyPresentIdentical]
    );
    assert_eq!(store.count_events(EventFilter::default()).await.unwrap(), 1);

    // Same id, different content: rejected as a conflict, not silently accepted.
    let mut mismatched = event.clone();
    mismatched.verb = "different-verb".to_string();
    let conflict = store
        .append_events_idempotent(vec![mismatched])
        .await
        .unwrap();
    assert_eq!(
        conflict.rows,
        vec![EventAppendDisposition::IdentityConflict]
    );

    // The stored row is untouched by the rejected conflicting retry.
    let stored = store.get_event(id).await.unwrap().unwrap();
    assert_eq!(stored.verb, "search");
}

#[tokio::test]
async fn identity_collision_is_per_row_and_preserves_unrelated_rows() {
    let store = setup_memory_store();

    let original = make_event("default");
    let original_id = original.id;
    store
        .append_events_idempotent(vec![original.clone()])
        .await
        .unwrap();

    let mut conflicting = original.clone();
    conflicting.verb = "changed".to_string();
    let fresh = make_event("default");
    let fresh_id = fresh.id;

    let result = store
        .append_events_idempotent(vec![conflicting, fresh])
        .await
        .unwrap();

    assert_eq!(
        result.rows,
        vec![
            EventAppendDisposition::IdentityConflict,
            EventAppendDisposition::Inserted,
        ],
        "one row's identity conflict must not affect an unrelated row in the same batch"
    );

    assert!(store.get_event(fresh_id).await.unwrap().is_some());
    let stored_original = store.get_event(original_id).await.unwrap().unwrap();
    assert_eq!(stored_original.verb, "search");
}

#[tokio::test]
async fn mixed_batch_store_failure_rolls_back_insertions() {
    let store = setup_memory_store();

    let good = make_event("default");
    let good_id = good.id;
    let bad = malformed_recall_event("default");

    let err = store
        .append_events_idempotent(vec![good, bad])
        .await
        .unwrap_err();
    let _ = err;

    assert!(
        store.get_event(good_id).await.unwrap().is_none(),
        "a genuine store failure later in the batch must roll back earlier insertions \
         in the same transaction, unlike a per-row identity conflict"
    );
    assert_eq!(store.count_events(EventFilter::default()).await.unwrap(), 0);
}

#[tokio::test]
async fn reply_loss_after_commit_retries_exactly_once() {
    let store = setup_memory_store();

    let event = make_event("default");
    let id = event.id;

    // Original request: the writer commits, but the caller never observes
    // the reply (WriterTaskRequestState::SideEffectsUnknown in production).
    let first = store
        .append_events_idempotent(vec![event.clone()])
        .await
        .unwrap();
    assert_eq!(first.rows, vec![EventAppendDisposition::Inserted]);

    // The caller retries through the idempotent method with the same
    // identity, per the contract's SideEffectsUnknown retry rule.
    let retry = store.append_events_idempotent(vec![event]).await.unwrap();
    assert_eq!(
        retry.rows,
        vec![EventAppendDisposition::AlreadyPresentIdentical]
    );

    assert_eq!(
        store.count_events(EventFilter::default()).await.unwrap(),
        1,
        "an ambiguous-ack retry must land exactly once, never a duplicate row"
    );
    assert!(store.get_event(id).await.unwrap().is_some());
}

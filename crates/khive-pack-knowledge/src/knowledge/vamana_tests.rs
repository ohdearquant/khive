use super::*;

mod helper_reuse {
    include!("vamana/helper_reuse_tests.rs");
}
use khive_runtime::KhiveRuntime;
use khive_storage::types::{SqlStatement, SqlValue};
use serde_json::json;

mod timing {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../test_support/timing.rs"
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn loaded_traversal_leaves_the_lexical_executor_runnable() {
    let ann = new_shared_for_role(false);
    let key = AnnKey::new("local", "test-model");
    let id = Uuid::new_v4();
    let pause = Arc::new(TestSearchPause::default());
    let mut bridge =
        AnnBridge::build(vec![1.0, 0.0, 0.0, 0.0], 4, vec![id]).expect("build one-vector bridge");
    bridge.search_pause = Some(Arc::clone(&pause));
    assert!(insert_ann_if_absent(&ann, key.clone(), bridge).await);

    let (cancel_watchdog, watchdog_cancelled) = std::sync::mpsc::channel();
    let watchdog_pause = Arc::clone(&pause);
    let watchdog = std::thread::spawn(move || {
        if watchdog_cancelled
            .recv_timeout(std::time::Duration::from_secs(2))
            .is_err()
        {
            watchdog_pause.release();
        }
    });

    let search = search_loaded_with_seq(&ann, &key, &[1.0, 0.0, 0.0, 0.0], 1);
    let lexical_progress = async {
        while !pause.started.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        let reached_before_release = !*pause.released.lock().expect("search pause");
        pause.release();
        reached_before_release
    };
    let (hits, progressed) = tokio::join!(search, lexical_progress);
    let _ = cancel_watchdog.send(());
    watchdog.join().expect("watchdog thread");
    assert!(progressed, "loaded traversal blocked the Tokio executor");
    assert_eq!(hits.expect("loaded bridge").0[0].0, id);
}

#[tokio::test(start_paused = true)]
#[serial_test::serial(background_tasks)]
async fn rotation_watcher_exits_on_local_shutdown_without_advancing_time() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rt = KhiveRuntime::new(khive_runtime::RuntimeConfig {
        db_path: Some(dir.path().join("rotation-shutdown.db")),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .expect("writable runtime");
    let ann = new_shared();
    let shutdown = tokio_util::sync::CancellationToken::new();
    let before = khive_runtime::background_task_count();
    let started_at = tokio::time::Instant::now();

    let watcher = start_rotation_watcher_with_shutdown(&rt, &ann, shutdown.clone())
        .expect("file-backed ANN state starts a watcher");
    assert!(rotation_watch_started_for_test(&ann));
    assert_eq!(khive_runtime::background_task_count(), before + 1);
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert_eq!(khive_runtime::background_task_count(), before + 1);

    assert!(
        !watcher.is_finished(),
        "uncancelled watcher must stay active"
    );

    shutdown.cancel();
    for _ in 0..100 {
        if watcher.is_finished() {
            break;
        }
        tokio::task::yield_now().await;
    }
    if !watcher.is_finished() {
        watcher.abort();
        let _ = watcher.await;
        panic!("local shutdown must stop the watcher while its ANN state is alive");
    }
    watcher
        .await
        .expect("rotation watcher must finish successfully after local shutdown");
    assert_eq!(khive_runtime::background_task_count(), before);
    assert_eq!(Arc::strong_count(&ann), 1);
    assert_eq!(tokio::time::Instant::now(), started_at);
}

#[test]
fn fresh_tail_merge_deduplicates_and_applies_deletes() {
    let deleted = Uuid::new_v4();
    let updated = Uuid::new_v4();
    let stable = Uuid::new_v4();
    let candidates = vec![(deleted, 0.99), (updated, 0.1), (stable, 0.5)];
    let ops = vec![(deleted, None), (updated, Some(vec![1.0, 0.0]))];

    let merged = merge_fresh_tail(candidates, &[1.0, 0.0], ops);

    assert!(
        !merged.iter().any(|(subject, _)| *subject == deleted),
        "a final tail delete must remove a stale ANN candidate"
    );
    assert_eq!(
        merged
            .iter()
            .filter(|(subject, _)| *subject == updated)
            .count(),
        1,
        "a tail upsert must replace, not duplicate, an ANN candidate"
    );
    assert_eq!(merged[0].0, updated, "the exact tail score must win");
    assert!(merged.iter().any(|(subject, _)| *subject == stable));
}

#[test]
fn fresh_tail_merge_breaks_equal_scores_by_uuid() {
    let low = Uuid::from_u128(1);
    let middle = Uuid::from_u128(2);
    let high = Uuid::from_u128(3);
    let ops = vec![
        (high, Some(vec![1.0, 0.0])),
        (low, Some(vec![1.0, 0.0])),
        (middle, Some(vec![1.0, 0.0])),
    ];

    let merged = merge_fresh_tail(Vec::new(), &[1.0, 0.0], ops);

    assert_eq!(
        merged
            .into_iter()
            .map(|(subject, _)| subject)
            .collect::<Vec<_>>(),
        vec![low, middle, high],
        "equal-cosine fresh hits must not inherit HashMap iteration order"
    );
}

#[tokio::test]
async fn fresh_tail_missing_registration_publishes_force_rebuild_sentinel() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let ann = new_shared();
    let namespace = "local";
    let model = "fresh-tail-registration-test";
    let key = AnnKey::new(namespace, model);

    let outcome = force_cold_after_registry_loss(&rt, &ann, &key).await;

    assert!(matches!(
        outcome,
        FreshTailOutcome::Replace { ref candidates, source_exhausted: true }
            if candidates.is_empty()
    ));
    assert_eq!(
        read_own_watermark(&rt, namespace, model)
            .await
            .expect("registry read"),
        Some(-1),
        "registry loss must publish the cross-process rebuild sentinel"
    );
    assert!(force_rebuild_required(&ann, &key));
}

#[tokio::test]
async fn read_only_fresh_tail_registry_loss_stays_process_local() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = khive_runtime::RuntimeConfig {
        db_path: Some(dir.path().join("read-only-fresh-tail.db")),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    };
    drop(KhiveRuntime::new(config.clone()).expect("migrate snapshot source"));
    #[cfg(unix)]
    {
        let db_path = config.db_path.as_ref().expect("db path");
        khive_storage::test_support::freeze_snapshot_sidecars(db_path);
    }
    let rt = KhiveRuntime::new_readonly(config).expect("open snapshot read-only");
    let ann = new_shared();
    let namespace = "local";
    let model = "read-only-fresh-tail-model";
    let key = AnnKey::new(namespace, model);
    let before = rt.backend().pool().writer_acquisition_snapshot();

    let outcome = force_cold_after_registry_loss(&rt, &ann, &key).await;

    assert!(matches!(
        outcome,
        FreshTailOutcome::Replace { ref candidates, source_exhausted: true }
            if candidates.is_empty()
    ));
    assert_eq!(
        read_own_watermark(&rt, namespace, model)
            .await
            .expect("registry read"),
        None,
        "read-only registry loss must not publish a rebuild sentinel"
    );
    assert!(
        !force_rebuild_required(&ann, &key),
        "read-only registry loss must not claim local rebuild authority"
    );
    assert_eq!(
        rt.backend().pool().writer_acquisition_snapshot(),
        before,
        "read-only fresh-tail degradation must not acquire a writer"
    );
}

#[tokio::test]
async fn force_rebuild_queries_do_not_retire_authoritative_warm() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let ann = new_shared();
    let key = AnnKey::new("local", "force-warm-test");
    force_cold_after_registry_loss(&rt, &ann, &key).await;
    simulate_warming_in_flight(&ann, key.clone());

    let outcome = fresh_tail_leg(&rt, &ann, &key, &[1.0], 1, None).await;

    assert!(matches!(
        outcome,
        FreshTailOutcome::Replace { ref candidates, source_exhausted: true }
            if candidates.is_empty()
    ));
    assert!(
        is_warming_not_loaded(&ann, &key),
        "a concurrent degraded query must not invalidate the authoritative warm"
    );
}

#[test]
fn reverse_map_for_tail_excludes_unrelated_and_tombstoned_owners() {
    let id_a = Uuid::new_v4();
    let id_b = Uuid::new_v4();
    let id_c = Uuid::new_v4();
    let absent = Uuid::new_v4();
    let vectors = vec![
        1.0, 0.0, 0.0, // old id_a, ordinal 0
        0.0, 1.0, 0.0, // id_b, ordinal 1
        0.0, 0.0, 1.0, // latest id_a, ordinal 2
        1.0, 1.0, 0.0, // unrelated id_c, ordinal 3
    ];
    let mut bridge =
        AnnBridge::build(vectors, 3, vec![id_a, id_b, id_a, id_c]).expect("build bridge");
    bridge.index.tombstone(0).expect("tombstone old id_a");
    bridge.index.tombstone(1).expect("tombstone id_b");

    let reverse = bridge.reverse_map_for([id_a, id_b, absent]);
    assert_eq!(reverse.len(), 1, "only requested live owners are indexed");
    assert_eq!(reverse.get(&id_a), Some(&2));
    assert!(!reverse.contains_key(&id_b), "tombstoned owner excluded");
    assert!(
        !reverse.contains_key(&id_c),
        "unrelated live owner excluded"
    );
}

/// #1150 regression: a tombstoned ordinal's stale id-map entry must not
/// let a later replay op for the old (already-deleted) subject tombstone
/// the slot a same-batch upsert just reused for a different subject.
#[test]
fn replay_does_not_tombstone_slot_reused_by_same_batch_upsert() {
    let id_a = Uuid::new_v4();
    let id_b = Uuid::new_v4();
    let id_c = Uuid::new_v4();

    let vectors = vec![
        1.0f32, 0.0, 0.0, // id_a, ordinal 0
        0.0, 1.0, 0.0, // id_b, ordinal 1
    ];
    let mut bridge = AnnBridge::build(vectors, 3, vec![id_a, id_b]).expect("build");

    // Simulate a PRIOR tombstone of id_a that left the id-map entry
    // stale (tombstoning never clears it) — exactly the persisted state
    // #1150 describes, without going through a save/load round trip.
    bridge.index.tombstone(0).expect("tombstone id_a");
    assert_eq!(
        bridge.id_map[0], id_a,
        "id-map entry stays stale after tombstone"
    );

    // Coalesced final tail: id_c's upsert (which recycles id_a's freed
    // ordinal 0) is processed BEFORE id_a's own final delete — a legal
    // op order since coalescing only guarantees per-subject dedup, not
    // cross-subject sequencing.
    let mut reverse = bridge.reverse_map_for([id_c, id_a]);
    bridge
        .apply_final_op(&mut reverse, id_c, Some(vec![0.0f32, 0.0, 1.0]))
        .expect("apply upsert");
    bridge
        .apply_final_op(&mut reverse, id_a, None)
        .expect("apply delete");

    assert_eq!(
        bridge.id_map[0], id_c,
        "ordinal 0 must be owned by id_c after the replay"
    );
    assert!(
        !bridge.index.is_tombstoned(0),
        "id_a's stale delete must not tombstone the slot id_c now owns"
    );
    let hits = bridge.search(&[0.0, 0.0, 1.0], 2);
    assert!(
        hits.iter().any(|(id, score)| *id == id_c && *score > 0.9),
        "id_c must remain live and searchable, got: {hits:?}"
    );
    assert!(
        !hits.iter().any(|(id, _)| *id == id_a),
        "id_a must not resurface as a search hit, got: {hits:?}"
    );
}

#[tokio::test]
async fn test_invalidate_snapshot_removes_vamana_rows() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let sql = rt.sql();

    let mut w = sql.writer().await.expect("writer");
    w.execute_script(
        "CREATE TABLE IF NOT EXISTS retrieval_snapshots (\
             namespace TEXT NOT NULL, index_type TEXT NOT NULL, \
             snapshot BLOB NOT NULL, created_at INTEGER NOT NULL, \
             PRIMARY KEY (namespace, index_type));"
            .into(),
    )
    .await
    .expect("create table");

    for (ns, idx_type) in &[
        ("local::vamana::model-a", "vamana"),
        ("local::vamana::model-b", "vamana"),
        ("local::hnsw::model-a", "hnsw"),
    ] {
        w.execute(SqlStatement {
                sql: "INSERT INTO retrieval_snapshots (namespace, index_type, snapshot, created_at) VALUES (?1, ?2, ?3, 0)".into(),
                params: vec![
                    SqlValue::Text(ns.to_string()),
                    SqlValue::Text(idx_type.to_string()),
                    SqlValue::Blob(b"{}".to_vec()),
                ],
                label: None,
            })
            .await
            .expect("insert");
    }
    drop(w);

    invalidate_snapshot(&rt, "local").await;

    let mut r = sql.reader().await.expect("reader");
    let rows = r
        .query_all(SqlStatement {
            sql: "SELECT namespace FROM retrieval_snapshots ORDER BY namespace".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("query");

    let remaining: Vec<String> = rows
        .iter()
        .filter_map(|row| match row.get("namespace") {
            Some(SqlValue::Text(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();

    assert!(
        remaining.contains(&"local::hnsw::model-a".to_string()),
        "HNSW rows must survive: {remaining:?}"
    );
    assert!(
        !remaining.contains(&"local::vamana::model-a".to_string()),
        "vamana model-a must be deleted: {remaining:?}"
    );
    assert!(
        !remaining.contains(&"local::vamana::model-b".to_string()),
        "vamana model-b must be deleted: {remaining:?}"
    );
}

#[tokio::test]
async fn test_invalidate_snapshot_does_not_cross_underscore_namespace() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let sql = rt.sql();

    let mut w = sql.writer().await.expect("writer");
    w.execute_script(
        "CREATE TABLE IF NOT EXISTS retrieval_snapshots (\
             namespace TEXT NOT NULL, index_type TEXT NOT NULL, \
             snapshot BLOB NOT NULL, created_at INTEGER NOT NULL, \
             PRIMARY KEY (namespace, index_type));"
            .into(),
    )
    .await
    .expect("create table");

    // "a_b" and "aXb" are distinct namespaces (the `_` in "a_b" is a
    // literal underscore, not a wildcard). Before #819's fix, invalidating
    // "a_b" also deleted "aXb"'s row because `_` is a single-character
    // LIKE wildcard.
    for ns in &["a_b::vamana::model-a", "aXb::vamana::model-a"] {
        w.execute(SqlStatement {
                sql: "INSERT INTO retrieval_snapshots (namespace, index_type, snapshot, created_at) VALUES (?1, ?2, ?3, 0)".into(),
                params: vec![
                    SqlValue::Text(ns.to_string()),
                    SqlValue::Text("vamana".to_string()),
                    SqlValue::Blob(b"{}".to_vec()),
                ],
                label: None,
            })
            .await
            .expect("insert");
    }
    drop(w);

    invalidate_snapshot(&rt, "a_b").await;

    let mut r = sql.reader().await.expect("reader");
    let rows = r
        .query_all(SqlStatement {
            sql: "SELECT namespace FROM retrieval_snapshots ORDER BY namespace".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("query");

    let remaining: Vec<String> = rows
        .iter()
        .filter_map(|row| match row.get("namespace") {
            Some(SqlValue::Text(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();

    assert!(
        remaining.contains(&"aXb::vamana::model-a".to_string()),
        "unrelated namespace 'aXb' must survive invalidating 'a_b': {remaining:?}"
    );
    assert!(
        !remaining.contains(&"a_b::vamana::model-a".to_string()),
        "'a_b' own snapshot must still be deleted: {remaining:?}"
    );
}

#[tokio::test]
async fn test_invalidate_snapshot_tolerates_missing_table() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    // No retrieval_snapshots table — must not panic.
    invalidate_snapshot(&rt, "local").await;
}

#[tokio::test]
async fn test_invalidate_clears_in_memory_ann() {
    let ann = new_shared();

    let dim = 4;
    let vectors = vec![1.0f32, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0];
    let ids = vec![Uuid::new_v4(), Uuid::new_v4()];
    let bridge = AnnBridge::build(vectors, dim, ids).expect("build");
    let key = AnnKey::new("local", "test-model");
    assert!(
        insert_ann_if_absent(&ann, key.clone(), bridge).await,
        "insert must succeed on empty cache"
    );
    assert!(
        ann.indexes.read().await.contains_key(&key),
        "pre-condition: ANN loaded"
    );

    clear_namespace(&ann, "local").await;
    assert!(
        !ann.indexes.read().await.contains_key(&key),
        "clearing SharedAnn must remove the bridge"
    );
}

#[tokio::test]
async fn shared_ann_is_keyed_by_namespace_and_model() {
    let ann = new_shared();
    let model = "test-model";
    let id_a = Uuid::new_v4();
    let id_b = Uuid::new_v4();

    let bridge_a = AnnBridge::build(vec![1.0, 0.0, 0.0, 0.0], 4, vec![id_a])
        .expect("build namespace A bridge");
    let bridge_b = AnnBridge::build(vec![0.0, 1.0, 0.0, 0.0], 4, vec![id_b])
        .expect("build namespace B bridge");

    assert!(insert_ann_if_absent(&ann, AnnKey::new("ns:a", model), bridge_a).await);
    assert!(insert_ann_if_absent(&ann, AnnKey::new("ns:b", model), bridge_b).await);

    let hits_b = search_loaded(&ann, &AnnKey::new("ns:b", model), &[1.0, 0.0, 0.0, 0.0], 1)
        .await
        .expect("namespace B bridge exists");

    assert_eq!(hits_b.len(), 1);
    assert_eq!(
        hits_b[0].0, id_b,
        "namespace B query must not return namespace A neighbour"
    );
}

// ── generation-checked install (issue #770) ──────────────────────────────

#[tokio::test]
async fn install_if_fresher_rejects_late_stale_build() {
    let ann = new_shared();
    let key = AnnKey::new("local", "test-model");

    let fresh = AnnBridge::build(vec![1.0, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
        .expect("build fresh bridge")
        .with_generation(2);
    let stale = AnnBridge::build(vec![0.0, 1.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
        .expect("build stale bridge")
        .with_generation(1);

    // Fresh install first, then a late-arriving stale build must not clobber it.
    install_if_fresher(&ann, &key, fresh).await;
    install_if_fresher(&ann, &key, stale).await;

    let installed_generation = ann
        .indexes
        .read()
        .await
        .get(&key)
        .expect("entry present")
        .generation;
    assert_eq!(
        installed_generation, 2,
        "stale build (generation 1) must not replace fresher installed entry (generation 2)"
    );
}

#[tokio::test]
async fn install_if_fresher_accepts_forward_progress() {
    let ann = new_shared();
    let key = AnnKey::new("local", "test-model");

    let old = AnnBridge::build(vec![1.0, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
        .expect("build old bridge")
        .with_generation(1);
    let newer = AnnBridge::build(vec![0.0, 1.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
        .expect("build newer bridge")
        .with_generation(2);

    // Normal forward progress: old installs first, newer build replaces it.
    install_if_fresher(&ann, &key, old).await;
    install_if_fresher(&ann, &key, newer).await;

    let installed_generation = ann
        .indexes
        .read()
        .await
        .get(&key)
        .expect("entry present")
        .generation;
    assert_eq!(
        installed_generation, 2,
        "newer build must replace an older installed entry"
    );
}

#[tokio::test]
async fn install_if_fresher_ties_keep_incumbent() {
    let ann = new_shared();
    let key = AnnKey::new("local", "test-model");

    let first = AnnBridge::build(vec![1.0, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
        .expect("build first bridge")
        .with_generation(1);
    let second_id = Uuid::new_v4();
    let second = AnnBridge::build(vec![0.0, 1.0, 0.0, 0.0], 4, vec![second_id])
        .expect("build second bridge")
        .with_generation(1);

    install_if_fresher(&ann, &key, first).await;
    install_if_fresher(&ann, &key, second).await;

    let hits = search_loaded(&ann, &key, &[0.0, 1.0, 0.0, 0.0], 1)
        .await
        .expect("entry present");
    assert_ne!(
        hits.first().map(|(id, _)| *id),
        Some(second_id),
        "equal-generation candidate must not replace the incumbent"
    );
}

#[tokio::test]
async fn install_if_fresher_installs_into_empty_slot() {
    let ann = new_shared();
    let key = AnnKey::new("local", "test-model");
    let bridge = AnnBridge::build(vec![1.0, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
        .expect("build bridge")
        .with_generation(0);

    install_if_fresher(&ann, &key, bridge).await;

    assert!(
        ann.indexes.read().await.contains_key(&key),
        "first successful build must always install into an empty slot"
    );
}

#[tokio::test]
async fn clear_namespace_bumps_generation_scoped_to_namespace() {
    let ann = new_shared();

    assert_eq!(current_generation(&ann, "ns:a"), 0);
    assert_eq!(current_generation(&ann, "ns:b"), 0);

    clear_namespace(&ann, "ns:a").await;

    assert_eq!(
        current_generation(&ann, "ns:a"),
        1,
        "clear_namespace must bump the invalidated namespace's generation"
    );
    assert_eq!(
        current_generation(&ann, "ns:b"),
        0,
        "clear_namespace must not affect a different namespace's generation"
    );
}

#[tokio::test]
async fn stale_build_installs_before_invalidation_race_is_rejected_after() {
    // Simulates the #770 race deterministically: build A (slow, e.g. the
    // full corpus rebuild fallthrough) starts scanning and captures its
    // generation floor. An invalidating write lands mid-build, clearing
    // the slot and bumping the namespace generation. The empty slot lets
    // a second, concurrent build B (e.g. `ensure_ann_background` retried
    // by the next search, since `clear_namespace` also freed the warming
    // guard) start, scan the now-current corpus, and install first. Only
    // afterward does build A's slow scan finish and attempt to install
    // its stale result — it must lose to B rather than clobbering it, the
    // exact bug this issue reports (`entry().or_insert()` would have let
    // A's late install win regardless of arrival order).
    let ann = new_shared();
    let key = AnnKey::new("local", "test-model");

    // Build A starts: capture the generation floor before doing any work.
    let build_a_generation = current_generation(&ann, "local");
    assert_eq!(build_a_generation, 0);

    // A concurrent write invalidates the namespace while A is still scanning.
    clear_namespace(&ann, "local").await;
    assert_eq!(current_generation(&ann, "local"), 1);

    // Build B starts after the invalidation (slot is empty, warming guard
    // was cleared too), scans the current corpus, and installs first.
    let build_b_generation = current_generation(&ann, "local");
    let build_b_id = Uuid::new_v4();
    let build_b_bridge = AnnBridge::build(vec![0.0, 1.0, 0.0, 0.0], 4, vec![build_b_id])
        .expect("build fresh bridge")
        .with_generation(build_b_generation);
    install_if_fresher(&ann, &key, build_b_bridge).await;
    assert!(
        ann.indexes.read().await.contains_key(&key),
        "build B (post-invalidation generation) must install"
    );

    // Build A's slow scan finally finishes and attempts to install its
    // stale (pre-invalidation) result.
    let build_a_bridge = AnnBridge::build(vec![1.0, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
        .expect("build stale bridge")
        .with_generation(build_a_generation);
    install_if_fresher(&ann, &key, build_a_bridge).await;

    let hits = search_loaded(&ann, &key, &[0.0, 1.0, 0.0, 0.0], 1)
        .await
        .expect("entry present");
    assert_eq!(
        hits.first().map(|(id, _)| *id),
        Some(build_b_id),
        "build A's late, stale install must not clobber build B's fresher result"
    );
}

#[tokio::test]
async fn stale_build_rejected_installing_into_still_empty_post_invalidation_slot() {
    // Deterministic reproduction of the #770 scenario through the EMPTY-SLOT
    // door (PR #815): unlike the test above (where a fresh build
    // B installs first, so the stale build has an incumbent to lose against),
    // this exercises the case where NOTHING has installed yet when the stale
    // build arrives. Build A captures its generation floor, an invalidating
    // write (`clear_namespace`) bumps the namespace's generation while the
    // slot is still empty, and only then does A's late, stale install attempt
    // land — straight into that still-empty slot. The old `install_if_fresher`
    // compared a candidate only against an *existing* entry, so an empty slot
    // meant nothing to compare against and the stale build installed
    // unconditionally. The fix compares against the namespace's CURRENT
    // generation instead, so this must be rejected even with no incumbent.
    let ann = new_shared();
    let key = AnnKey::new("local", "test-model");

    // Build A starts: capture the generation floor before doing any work.
    let build_a_generation = current_generation(&ann, "local");
    assert_eq!(build_a_generation, 0);

    // An invalidating write lands while A is still scanning. The slot was
    // never populated, so this is a no-op on the map, but it must still
    // bump the namespace's generation.
    clear_namespace(&ann, "local").await;
    assert_eq!(current_generation(&ann, "local"), 1);
    assert!(
        !ann.indexes.read().await.contains_key(&key),
        "precondition: slot must still be empty after clear_namespace"
    );

    // Build A's slow scan finally finishes and attempts to install its
    // stale (pre-invalidation) result into the still-empty slot.
    let build_a_bridge = AnnBridge::build(vec![1.0, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
        .expect("build stale bridge")
        .with_generation(build_a_generation);
    install_if_fresher(&ann, &key, build_a_bridge).await;

    assert!(
        !ann.indexes.read().await.contains_key(&key),
        "stale pre-invalidation build must not install into the emptied slot, \
             even with no incumbent to compare against"
    );
    assert!(
        search_loaded(&ann, &key, &[1.0, 0.0, 0.0, 0.0], 1)
            .await
            .is_none(),
        "the fast path must not serve a stale index that was correctly rejected at install"
    );
}

// ── is_warming_not_loaded ─────────────────────────────────────────────────

#[test]
fn is_warming_false_when_neither_warming_nor_loaded() {
    let ann = new_shared();
    let key = AnnKey::new("local", "test-model");
    assert!(
        !is_warming_not_loaded(&ann, &key),
        "key absent from both sets must return false"
    );
}

#[test]
fn is_warming_true_when_in_warming_but_not_indexes() {
    let ann = new_shared();
    let key = AnnKey::new("local", "test-model");
    simulate_warming_in_flight(&ann, key.clone());
    assert!(
        is_warming_not_loaded(&ann, &key),
        "key in warming but not indexes must return true"
    );
}

#[tokio::test]
async fn is_warming_false_when_both_warming_and_loaded() {
    let ann = new_shared();
    let key = AnnKey::new("local", "test-model");
    // Mark as warming.
    simulate_warming_in_flight(&ann, key.clone());
    // Now insert the index (simulates background warm completing).
    let bridge =
        AnnBridge::build(vec![1.0f32, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()]).expect("build");
    insert_ann_if_absent(&ann, key.clone(), bridge).await;
    assert!(
        !is_warming_not_loaded(&ann, &key),
        "key in both warming and indexes must return false (warm is done)"
    );
}

// ── wait_ready ────────────────────────────────────────────────────────────

#[tokio::test]
async fn wait_ready_returns_true_immediately_when_already_loaded() {
    let ann = new_shared();
    let key = AnnKey::new("local", "test-model");
    let bridge =
        AnnBridge::build(vec![1.0f32, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()]).expect("build");
    insert_ann_if_absent(&ann, key.clone(), bridge).await;
    // Already loaded — should return true without sleeping.
    let ready = wait_ready(&ann, &key, 100, 10).await;
    assert!(ready, "must return true when index is already in the map");
}

#[tokio::test]
async fn wait_ready_returns_false_on_timeout_when_never_loaded() {
    let ann = new_shared();
    let key = AnnKey::new("local", "test-model");
    // Nothing inserted — should time out and return false.
    let ready = wait_ready(&ann, &key, 60, 10).await;
    assert!(
        !ready,
        "must return false when index never appears within timeout"
    );
}

#[tokio::test]
async fn wait_ready_returns_true_when_index_appears_mid_poll() {
    let ann = new_shared();
    let key = AnnKey::new("local", "test-model");
    let ann2 = ann.clone();
    let key2 = key.clone();
    // Spawn a task that inserts the bridge after a short delay.
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        let bridge =
            AnnBridge::build(vec![1.0f32, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()]).expect("build");
        insert_ann_if_absent(&ann2, key2, bridge).await;
    });
    // Poll with a 500ms timeout; the insert happens at ~40ms so it should succeed.
    let ready = wait_ready(&ann, &key, 500, 10).await;
    assert!(ready, "must return true when index appears before timeout");
}

// ── unavailable marker: terminal warm outcome (issue #1026) ──────────────

fn assert_terminal_wait_latency(elapsed: std::time::Duration) {
    let bound = timing::duration_bound(
        std::time::Duration::from_millis(ANN_WARM_WAIT_POLL_MS * 10),
        Some(std::time::Duration::from_millis(
            ANN_WARM_WAIT_TIMEOUT_MS / 2,
        )),
    )
    .expect("terminal wait has a numeric bound in both test tiers");
    assert!(
        elapsed < bound,
        "terminal unavailable outcome must short-circuit within {bound:?}: {elapsed:?}"
    );
}

#[tokio::test]
async fn wait_ready_returns_false_immediately_when_marked_unavailable() {
    let ann = new_shared();
    let key = AnnKey::new("local", "test-model");
    mark_unavailable(&ann, &key, current_generation(&ann, "local"));

    let start = std::time::Instant::now();
    // Timeout is generous (matching production ANN_WARM_WAIT_TIMEOUT_MS)
    // to prove the short-circuit fires rather than the deadline.
    let ready = wait_ready(&ann, &key, ANN_WARM_WAIT_TIMEOUT_MS, ANN_WARM_WAIT_POLL_MS).await;
    let elapsed = start.elapsed();

    assert!(
        !ready,
        "must return false for a key marked unavailable at the current generation"
    );
    assert_terminal_wait_latency(elapsed);
}

#[tokio::test]
async fn wait_ready_resumes_polling_when_unavailable_marker_is_stale() {
    let ann = new_shared();
    let key = AnnKey::new("local", "test-model");

    // Mark unavailable at generation 0, then bump the namespace's
    // generation past it so the marker is stale on the next check.
    mark_unavailable(&ann, &key, 0);
    clear_namespace(&ann, "local").await;
    assert_eq!(current_generation(&ann, "local"), 1);

    let ann2 = ann.clone();
    let key2 = key.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        let bridge = AnnBridge::build(vec![1.0f32, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
            .expect("build")
            .with_generation(1);
        install_if_fresher(&ann2, &key2, bridge).await;
    });

    let ready = wait_ready(&ann, &key, 500, 10).await;
    assert!(
        ready,
        "a stale unavailable marker must not block polling; the index installed \
             mid-poll must still be observed"
    );
}

#[tokio::test]
async fn install_if_fresher_clears_unavailable_marker_on_successful_install() {
    let ann = new_shared();
    let key = AnnKey::new("local", "test-model");
    mark_unavailable(&ann, &key, 0);

    let bridge = AnnBridge::build(vec![1.0f32, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
        .expect("build")
        .with_generation(0);
    install_if_fresher(&ann, &key, bridge).await;

    assert!(
        !unavailable_guard(&ann.unavailable).contains_key(&key),
        "a successful install must clear the unavailable marker for its key"
    );
}

#[tokio::test]
async fn install_if_fresher_stale_reject_does_not_clear_unavailable_marker() {
    let ann = new_shared();
    let key = AnnKey::new("local", "test-model");

    // Bump the namespace generation past the marker AND past the candidate,
    // so install_if_fresher rejects the candidate outright.
    clear_namespace(&ann, "local").await;
    mark_unavailable(&ann, &key, current_generation(&ann, "local"));

    let stale = AnnBridge::build(vec![1.0f32, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
        .expect("build")
        .with_generation(0);
    install_if_fresher(&ann, &key, stale).await;

    assert!(
        !ann.indexes.read().await.contains_key(&key),
        "stale candidate must not install"
    );
    assert!(
        unavailable_guard(&ann.unavailable).contains_key(&key),
        "a rejected (non-installed) candidate must not clear the unavailable marker"
    );
}

// ── poison recovery ───────────────────────────────────────────────────────

/// Poison the warm-state Mutex by panicking while holding the guard, then
/// verify that `warm_states_guard` and callers built on it survive and
/// return sane results.
///
/// This test WOULD panic if `warm_states_guard` were reverted to
/// `.expect("warm-state lock")`, because a poisoned Mutex causes `lock()`
/// to return `Err`, and `.expect()` converts that to a panic.
#[test]
fn warm_states_guard_recovers_from_poison() {
    let ann = new_shared();
    let key = AnnKey::new("poison-ns", "poison-model");

    // Poison the mutex by sharing the Ann via Arc across a thread that panics
    // while holding the guard.
    let ann2 = ann.clone();
    let join_result = std::thread::spawn(move || {
        let _guard = ann2.warm_states.lock().expect("pre-poison lock");
        panic!("deliberate poison");
    })
    .join();
    assert!(join_result.is_err(), "poison thread must have panicked");
    assert!(
        ann.warm_states.is_poisoned(),
        "mutex must be poisoned before recovery"
    );

    // `warm_states_guard` must recover the guard without panicking.
    let guard = warm_states_guard(&ann.warm_states);
    assert!(
        !guard.contains_key(&key),
        "recovered guard must report key absent"
    );
    drop(guard);

    // Higher-level callers built on `warm_states_guard` must also succeed.
    assert!(
        !is_warming_not_loaded(&ann, &key),
        "is_warming_not_loaded must not panic on poisoned Mutex"
    );
}

// ── shared warm-state machine (issue #566) ────────────────────────────────

#[tokio::test]
async fn warm_state_success_becomes_ready_and_suppresses_duplicates() {
    let ann = new_shared();
    let key = AnnKey::new("local", "warm-unify-model");

    let permit = begin_warm(&ann, key.clone()).expect("Absent -> Warming");
    assert!(
        is_warming_not_loaded(&ann, &key),
        "owned Warming state without an index must report in flight"
    );
    assert!(
        begin_warm(&ann, key.clone()).is_none(),
        "a current Warming owner must singleflight duplicate callers"
    );

    let bridge = AnnBridge::build(vec![1.0f32, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
        .expect("build bridge for warm-path test");
    insert_ann_if_absent(&ann, key.clone(), bridge).await;
    finish_warm(permit, AnnWarmOutcome::Ready).await;

    assert!(
        !is_warming_not_loaded(&ann, &key),
        "Ready state must not report warming in flight"
    );
    assert!(
        matches!(
            warm_states_guard(&ann.warm_states).get(&key),
            Some(AnnWarmState::Ready { generation: 0 })
        ),
        "a published index must finish in Ready"
    );
    assert!(
        begin_warm(&ann, key).is_none(),
        "Ready at the current generation must suppress redundant loads"
    );
}

#[tokio::test]
async fn warm_state_failed_and_empty_outcomes_remain_retryable() {
    let ann = new_shared();
    let key = AnnKey::new("local", "warm-unify-fail-model");

    let failed = begin_warm(&ann, key.clone()).expect("first warm");
    finish_warm(failed, AnnWarmOutcome::Failed).await;
    assert!(
        matches!(
            warm_states_guard(&ann.warm_states).get(&key),
            Some(AnnWarmState::Failed {
                error: AnnWarmFailure::Operational,
                ..
            })
        ),
        "no index and no empty marker is an operational failure"
    );

    let empty_retry = begin_warm(&ann, key.clone()).expect("Failed -> Warming retry");
    mark_unavailable(&ann, &key, current_generation(&ann, "local"));
    finish_warm(empty_retry, AnnWarmOutcome::Empty).await;
    assert!(
        matches!(
            warm_states_guard(&ann.warm_states).get(&key),
            Some(AnnWarmState::Failed {
                error: AnnWarmFailure::EmptyCorpus,
                ..
            })
        ),
        "current-generation empty scan must retain its distinct failure reason"
    );

    assert!(
        !is_warming_not_loaded(&ann, &key),
        "Failed must not masquerade as an in-flight warm"
    );
    let retry = begin_warm(&ann, key).expect("empty failures must remain retryable");
    drop(retry);
}

#[tokio::test]
async fn failed_replacement_stays_retryable_with_servable_stale_index() {
    let ann = new_shared();
    let key = AnnKey::new("local", "warm-stale-retry-model");
    let permit = begin_warm(&ann, key.clone()).expect("replacement warm");

    // ADR-079 rule 8 serves a stale bridge while its replacement rebuilds.
    let stale = AnnBridge::build(vec![1.0f32, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
        .expect("build stale bridge");
    insert_ann_if_absent(&ann, key.clone(), stale).await;
    finish_warm(permit, AnnWarmOutcome::Failed).await;

    assert!(
        search_loaded(&ann, &key, &[1.0, 0.0, 0.0, 0.0], 1)
            .await
            .is_some(),
        "the stale fallback must remain available to search"
    );
    assert!(
        begin_warm(&ann, key).is_some(),
        "a failed replacement must retry despite the servable stale fallback"
    );
}

#[tokio::test]
async fn stale_finish_cannot_steal_new_post_invalidation_owner() {
    let ann = new_shared();
    let key = AnnKey::new("local", "warm-owner-model");

    let stale = begin_warm(&ann, key.clone()).expect("warm A");
    clear_namespace(&ann, "local").await;
    let current = begin_warm(&ann, key.clone()).expect("warm B after invalidation");
    let current_id = current.attempt_id;

    finish_warm(stale, AnnWarmOutcome::Failed).await;
    assert!(
        matches!(
            warm_states_guard(&ann.warm_states).get(&key),
            Some(AnnWarmState::Warming { attempt_id, .. }) if *attempt_id == current_id
        ),
        "warm A's late cleanup must not erase or complete warm B's ownership"
    );
    assert!(
        begin_warm(&ann, key.clone()).is_none(),
        "warm B must remain the only current singleflight owner"
    );

    finish_warm(current, AnnWarmOutcome::Failed).await;
    assert!(
        begin_warm(&ann, key).is_some(),
        "warm B's failed completion must make the slot retryable"
    );
}

#[test]
fn dropped_warm_permit_cannot_leave_stale_warming_ownership() {
    let ann = new_shared();
    let key = AnnKey::new("local", "warm-cancel-model");

    let permit = begin_warm(&ann, key.clone()).expect("warm attempt");
    drop(permit);

    assert!(
        matches!(
            warm_states_guard(&ann.warm_states).get(&key),
            Some(AnnWarmState::Failed {
                error: AnnWarmFailure::Interrupted,
                ..
            })
        ),
        "cancellation must transition the owned slot out of Warming"
    );
    assert!(
        begin_warm(&ann, key).is_some(),
        "an interrupted warm must be retryable"
    );
}

// ── AnnBridge::save_atomic / load (slice 1b-i, ADR-079) ──────────────────

fn build_test_bridge(dim: usize, n: usize) -> (AnnBridge, Vec<Uuid>) {
    let ids: Vec<Uuid> = (0..n).map(|_| Uuid::new_v4()).collect();
    let mut vectors: Vec<f32> = Vec::with_capacity(n * dim);
    for i in 0..n {
        for d in 0..dim {
            vectors.push(if d == i % dim { 1.0 } else { 0.0 });
        }
    }
    let bridge = AnnBridge::build(vectors, dim, ids.clone()).expect("build test bridge");
    (bridge, ids)
}

/// A peer rotation must be observed without a search request. The two
/// checkpoints below have identical vector/graph/lifecycle bytes; their
/// UUID sidecars differ, which also proves the replacement mapping is the
/// one served after the watcher tick (#2081).
#[tokio::test]
async fn rotation_tick_releases_predecessor_and_adopts_identical_peer_checkpoint() {
    let temp = TempDir::new().expect("tempdir");
    let rt = file_rt_with_embedder(temp.path().join("rotation.db"));
    let ann = new_shared();
    let key = AnnKey::new("local", WARM_TEST_MODEL);
    let segment_dir =
        ann_segment_dir(&rt, "local", WARM_TEST_MODEL).expect("file-backed segment directory");

    let (first, first_ids) = build_test_bridge(4, 2);
    first
        .save_atomic(&segment_dir)
        .expect("persist first generation");
    let mut loaded = AnnBridge::load(&segment_dir)
        .expect("load first mmap generation")
        .with_generation(7);
    let probe = Arc::new(());
    let dropped = Arc::downgrade(&probe);
    loaded.drop_probe = Some(probe);
    assert!(install_replacing(&ann, &key, loaded).await);
    let first_digest = ann
        .indexes
        .read()
        .await
        .get(&key)
        .and_then(|bridge| bridge.commit_digest)
        .expect("loaded bridge identity");

    let (second, second_ids) = build_test_bridge(4, 2);
    assert_ne!(first_ids, second_ids, "fixture sidecars must differ");
    second
        .save_atomic(&segment_dir)
        .expect("rotate identical checkpoint");

    refresh_rotated_segments_once(&rt, &ann).await;

    assert!(
        dropped.upgrade().is_none(),
        "replacing the cache entry must drop the predecessor mmap owner"
    );
    let installed = ann.indexes.read().await;
    let bridge = installed.get(&key).expect("rotated bridge installed");
    assert_ne!(bridge.commit_digest, Some(first_digest));
    assert_eq!(
        bridge.search(&[1.0, 0.0, 0.0, 0.0], 1)[0].0,
        second_ids[0],
        "the rotated UUID sidecar must be the served mapping"
    );
    assert_eq!(bridge.generation, 7, "local generation fence is preserved");
}

async fn install_generation_then_publish_invalid_rotation(
    temp: &TempDir,
) -> (KhiveRuntime, SharedAnn, AnnKey) {
    let rt = file_rt_with_embedder(temp.path().join("rotation.db"));
    let ann = new_shared();
    let key = AnnKey::new("local", WARM_TEST_MODEL);
    let segment_dir =
        ann_segment_dir(&rt, "local", WARM_TEST_MODEL).expect("file-backed segment directory");

    let (first, _) = build_test_bridge(4, 2);
    first
        .save_atomic(&segment_dir)
        .expect("persist first generation");
    let loaded = AnnBridge::load(&segment_dir)
        .expect("load first mmap generation")
        .with_generation(7);
    assert!(install_replacing(&ann, &key, loaded).await);

    // A peer publishes a changed commit whose UUID sidecar is missing, so
    // the rotated generation fails validation and the incumbent is evicted.
    let (second, _) = build_test_bridge(4, 2);
    second.save_atomic(&segment_dir).expect("rotate checkpoint");
    std::fs::remove_file(segment_dir.join("external_ids.bin")).expect("remove sidecar");

    (rt, ann, key)
}

#[tokio::test]
async fn invalid_rotation_keeps_in_flight_warming_state() {
    let temp = TempDir::new().expect("tempdir");
    let (rt, ann, key) = install_generation_then_publish_invalid_rotation(&temp).await;

    // A concurrent request owns the single-flight slot for this key.
    warm_states_guard(&ann.warm_states).insert(
        key.clone(),
        AnnWarmState::Warming {
            attempt_id: 41,
            generation: 7,
            started_at: std::time::Instant::now(),
        },
    );

    refresh_rotated_segments_once(&rt, &ann).await;

    assert!(
        ann.indexes.read().await.get(&key).is_none(),
        "an invalid rotated generation must evict the incumbent"
    );
    assert!(
        matches!(
            warm_states_guard(&ann.warm_states).get(&key),
            Some(AnnWarmState::Warming { attempt_id: 41, .. })
        ),
        "eviction must not clear a warm state owned by an in-flight permit"
    );
}

#[tokio::test]
async fn invalid_rotation_clears_ready_state_of_evicted_bridge() {
    let temp = TempDir::new().expect("tempdir");
    let (rt, ann, key) = install_generation_then_publish_invalid_rotation(&temp).await;

    warm_states_guard(&ann.warm_states).insert(key.clone(), AnnWarmState::Ready { generation: 7 });

    refresh_rotated_segments_once(&rt, &ann).await;

    assert!(
        ann.indexes.read().await.get(&key).is_none(),
        "an invalid rotated generation must evict the incumbent"
    );
    assert!(
        warm_states_guard(&ann.warm_states).get(&key).is_none(),
        "the Ready state described the evicted bridge and must go with it"
    );
}

#[tokio::test]
async fn invalid_rotation_does_not_clear_a_newer_builds_ready_state() {
    let temp = TempDir::new().expect("tempdir");
    let (rt, ann, key) = install_generation_then_publish_invalid_rotation(&temp).await;

    // A later build's permit already published Ready for this key at a
    // newer generation than the one this eviction is evicting; sharing
    // the key must not make the cleanup sweep it up too.
    warm_states_guard(&ann.warm_states).insert(key.clone(), AnnWarmState::Ready { generation: 8 });

    refresh_rotated_segments_once(&rt, &ann).await;

    assert!(
        ann.indexes.read().await.get(&key).is_none(),
        "an invalid rotated generation must still evict the incumbent"
    );
    assert!(
        matches!(
            warm_states_guard(&ann.warm_states).get(&key),
            Some(AnnWarmState::Ready { generation: 8 })
        ),
        "eviction must not clear a later build's Ready state sharing the key"
    );
}

/// Regression for issue #2340: `finish_warm`'s Ready decision (read
/// `indexes`, check the bridge's generation) and its publication into
/// `warm_states` used to be two separate critical sections, so a
/// rotation eviction could run its own two-step remove-then-cleanup in
/// between, observe the state still `Warming`, and skip the cleanup —
/// leaving `warm_states` say `Ready` for a key `indexes` no longer has a
/// bridge for. The invariant under test: `finish_warm` holds the
/// `indexes` read guard from its Ready decision through the publication
/// into `warm_states`, so an eviction spawned in between (here through
/// `run_finish_warm_ready_test_hook`) cannot complete before the publish
/// and the two maps never disagree.
#[tokio::test]
async fn finish_warm_ready_publish_is_atomic_with_concurrent_eviction() {
    let temp = TempDir::new().expect("tempdir");
    let (_rt, ann, key) = install_generation_then_publish_invalid_rotation(&temp).await;

    let incumbent_digest = ann
        .indexes
        .read()
        .await
        .get(&key)
        .and_then(|bridge| bridge.commit_digest)
        .expect("incumbent bridge installed with a commit identity");

    let permit = begin_warm(&ann, key.clone()).expect("a fresh key grants a warm permit");

    let (handle_tx, mut handle_rx) = tokio::sync::mpsc::unbounded_channel();
    *ann.test_finish_warm_ready_hook.lock().unwrap() = Some(Arc::new(FinishWarmReadyTestHook {
        incumbent_digest,
        generation: 7,
        handle_tx,
    }));

    // Drives finish_warm's Ready publish. Its test hook spawns
    // `evict_bridge_and_ready_state` — the same critical section the
    // rotation watcher's `Err` arm uses — against the identical
    // incumbent bridge while finish_warm still holds the `indexes` read
    // guard it used for the Ready decision.
    finish_warm(permit, AnnWarmOutcome::Ready).await;

    let evict_handle = handle_rx
        .recv()
        .await
        .expect("the ready hook spawns the eviction task");
    evict_handle.await.expect("eviction task completes");

    // Invariant: a Ready state for the key implies a bridge is
    // installed for the key. The two sides of the race can settle
    // either way — the eviction's write lock was blocked until
    // finish_warm published and can then see and clear the Ready state
    // it just orphaned — but never in the inconsistent middle: Ready
    // with no bridge.
    let is_ready = matches!(
        warm_states_guard(&ann.warm_states).get(&key),
        Some(AnnWarmState::Ready { .. })
    );
    if is_ready {
        assert!(
            ann.indexes.read().await.get(&key).is_some(),
            "a Ready warm state must not describe a bridge the rotation watcher evicted"
        );
    }
}

#[test]
fn ann_bridge_save_atomic_load_round_trip() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let dim = 4;
    let (bridge, ids) = build_test_bridge(dim, 4);
    let first_id = ids[0];

    bridge.save_atomic(dir.path()).expect("save_atomic");

    let loaded = AnnBridge::load(dir.path()).expect("load");
    assert_eq!(
        loaded.num_vectors(),
        bridge.num_vectors(),
        "loaded vector count must match saved"
    );

    // Search with a query that points at vector 0 (1.0, 0.0, 0.0, 0.0)
    let query = vec![1.0f32, 0.0, 0.0, 0.0];
    let hits = loaded.search(&query, 1);
    assert_eq!(hits.len(), 1, "must return 1 hit");
    assert_eq!(hits[0].0, first_id, "top hit must be the first UUID");
}

#[test]
fn ann_bridge_load_missing_sidecar_err() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let (bridge, _) = build_test_bridge(4, 2);

    bridge.save_atomic(dir.path()).expect("save_atomic");
    std::fs::remove_file(dir.path().join("external_ids.bin")).expect("remove sidecar");

    let result = AnnBridge::load(dir.path());
    assert!(
        result.is_err(),
        "load must fail when external_ids.bin is missing"
    );
}

#[test]
fn ann_bridge_load_torn_pair_err() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let dim = 4;

    // Save bridge A into the directory — both segments and sidecar for A.
    let (bridge_a, _) = build_test_bridge(dim, 2);
    bridge_a.save_atomic(dir.path()).expect("save_atomic A");

    // Overwrite the Vamana segments with bridge B's segments ONLY (no sidecar update).
    // This simulates a crash after VamanaIndex::save_atomic but before write_external_ids_sidecar.
    let (bridge_b, _) = build_test_bridge(dim, 3);
    bridge_b
        .index
        .save_atomic(dir.path())
        .expect("save_atomic B segments");

    // Now: metadata.bin is B's commit record, external_ids.bin is still bound
    // to A's commit-record digest.
    let result = AnnBridge::load(dir.path());
    assert!(
        result.is_err(),
        "load must fail when sidecar commit digest != on-disk commit record (torn pair)"
    );
    let err = result.err().expect("already asserted is_err");
    assert!(
        err.contains("commit-digest mismatch") || err.contains("torn"),
        "error message must mention digest mismatch or torn pair, got: {err}"
    );
}

#[test]
fn ann_bridge_load_count_mismatch_err() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let (bridge, _) = build_test_bridge(4, 2);
    bridge.save_atomic(dir.path()).expect("save_atomic");

    // Rewrite the sidecar via the codec itself: correctly bound to the
    // on-disk commit record, internally consistent, but carrying 99 UUIDs
    // instead of the index's 2 — only the count cross-check can catch it.
    let digest = segment_commit_digest(dir.path())
        .expect("digest ok")
        .expect("commit record present");
    let wrong_ids: Vec<uuid::Uuid> = (0..99).map(|_| uuid::Uuid::new_v4()).collect();
    write_external_ids_sidecar(dir.path(), &digest, &wrong_ids).expect("write patched sidecar");

    let result = AnnBridge::load(dir.path());
    assert!(
        result.is_err(),
        "load must fail when sidecar count != index.num_vectors()"
    );
}

#[test]
fn ann_bridge_load_bad_magic_err() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let (bridge, _) = build_test_bridge(4, 2);
    bridge.save_atomic(dir.path()).expect("save_atomic");

    // Overwrite the first 8 bytes with a wrong magic.
    let mut sidecar_bytes =
        std::fs::read(dir.path().join("external_ids.bin")).expect("read sidecar");
    sidecar_bytes[0..8].copy_from_slice(b"WRONGMAG");
    std::fs::write(dir.path().join("external_ids.bin"), &sidecar_bytes)
        .expect("write bad-magic sidecar");

    let result = AnnBridge::load(dir.path());
    assert!(
        result.is_err(),
        "load must fail when external_ids.bin has wrong magic"
    );
    let err = result.err().expect("already asserted is_err");
    assert!(
        err.contains("magic"),
        "error must mention magic mismatch, got: {err}"
    );
}

// ── slice 1b-ii-a: warm-path tests (ADR-079) ─────────────────────────────

use async_trait::async_trait;
use khive_runtime::{AllowAllGate, BackendId, EmbedderProvider, RuntimeConfig};
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use tempfile::TempDir;

const WARM_TEST_MODEL: &str = "all-minilm-l6-v2";
const WARM_DIMS: usize = 384;

struct ConstVecService;

#[async_trait]
impl EmbeddingService for ConstVecService {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> std::result::Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| vec![1.0_f32; WARM_DIMS]).collect())
    }

    fn supports_model(&self, _: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "const-vec"
    }
}

struct TestEmbedderProvider;

#[async_trait]
impl EmbedderProvider for TestEmbedderProvider {
    fn name(&self) -> &str {
        WARM_TEST_MODEL
    }

    fn dimensions(&self) -> usize {
        WARM_DIMS
    }

    async fn build(&self) -> khive_runtime::RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(Arc::new(ConstVecService))
    }
}

fn rt_with_embedder(db_path: Option<std::path::PathBuf>) -> KhiveRuntime {
    let rt = KhiveRuntime::new(RuntimeConfig {
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: Default::default(),
        wal_ceiling_env_raw: None,
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        brain: Default::default(),
        git_write: Default::default(),
        display_timezone: khive_runtime::config::resolve_default_display_timezone(),
        events_split: None,
        // A file runtime takes the lock namespace beside its database, so its
        // migrations and writes do not queue behind other tests' on this
        // volume; a child process reopening the same file shares it.
        volume_lock_dir: db_path
            .as_deref()
            .and_then(std::path::Path::parent)
            .map(|dir| dir.join("volume-locks"))
            .or_else(|| khive_runtime::RuntimeConfig::no_embeddings().volume_lock_dir),
        db_path,
        blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::local(),
        embedding_model: Some(EmbeddingModel::AllMiniLmL6V2),
        additional_embedding_models: vec![],
        gate: Arc::new(AllowAllGate),
        packs: vec!["kg".to_string(), "knowledge".to_string()],
        backend_id: BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: None,
        exec: Default::default(),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .expect("test runtime");
    rt.register_embedder(TestEmbedderProvider);
    rt
}

fn file_rt_with_embedder(db_path: std::path::PathBuf) -> KhiveRuntime {
    rt_with_embedder(Some(db_path))
}

fn memory_rt_with_embedder() -> KhiveRuntime {
    rt_with_embedder(None)
}

/// Seed `n` distinct rows into the vec0 table for `WARM_TEST_MODEL`.
///
/// Calls `rt.vectors_for_model` first so the virtual table is created, then
/// inserts raw f32 LE blobs directly via SQL.
async fn seed_warm_corpus(rt: &KhiveRuntime, token: &NamespaceToken, n: usize) {
    seed_warm_corpus_opts(rt, token, n, true).await;
}

/// `log = false` seeds vec rows WITHOUT write-log rows — constructs the
/// empty-log zero-watermark baseline state (a corpus that predates the
/// migration's first logged write).
async fn seed_warm_corpus_opts(rt: &KhiveRuntime, token: &NamespaceToken, n: usize, log: bool) {
    let _store = rt
        .vectors_for_model(token, WARM_TEST_MODEL)
        .expect("vec store");
    let model_key = sanitize_model_key(WARM_TEST_MODEL);
    let table = format!("vec_{model_key}");
    let ns = token.namespace().as_str().to_owned();
    let sql = rt.sql();
    let mut w = sql.writer().await.expect("writer");
    for i in 0..n {
        let id = Uuid::new_v4();
        let mut v = [0.0_f32; WARM_DIMS];
        v[i % WARM_DIMS] = 1.0;
        let bytes: Vec<u8> = v.iter().flat_map(|f| f.to_le_bytes()).collect();
        w.execute(SqlStatement {
            sql: format!(
                "INSERT INTO {table} \
                     (subject_id, namespace, kind, field, embedding_model, embedding) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
            ),
            params: vec![
                SqlValue::Text(id.to_string()),
                SqlValue::Text(ns.clone()),
                SqlValue::Text("concept".to_string()),
                SqlValue::Text("knowledge.atom".to_string()),
                SqlValue::Text(WARM_TEST_MODEL.to_string()),
                SqlValue::Blob(bytes),
            ],
            label: None,
        })
        .await
        .expect("insert corpus row");
        if !log {
            continue;
        }
        // Mirror the production write path: every vector mutation appends
        // a write-log row in the same funnel (ADR-079 Amendment 1).
        w.execute(SqlStatement {
            sql: "INSERT INTO ann_write_log \
                      (namespace, embedding_model, kind, field, subject_id, op) \
                      VALUES (?1, ?2, ?3, ?4, ?5, 'upsert')"
                .into(),
            params: vec![
                SqlValue::Text(ns.clone()),
                SqlValue::Text(WARM_TEST_MODEL.to_string()),
                SqlValue::Text("concept".to_string()),
                SqlValue::Text("knowledge.atom".to_string()),
                SqlValue::Text(id.to_string()),
            ],
            label: None,
        })
        .await
        .expect("append write-log row");
    }
}

async fn append_warm_vector(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    embedding: [f32; WARM_DIMS],
) -> Uuid {
    let _store = rt
        .vectors_for_model(token, WARM_TEST_MODEL)
        .expect("vec store");
    let table = format!("vec_{}", sanitize_model_key(WARM_TEST_MODEL));
    let namespace = token.namespace().as_str().to_owned();
    let subject = Uuid::new_v4();
    let bytes: Vec<u8> = embedding
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    let sql = rt.sql();
    let mut writer = sql.writer().await.expect("writer");
    writer
        .execute(SqlStatement {
            sql: format!(
                "INSERT INTO {table} \
                     (subject_id, namespace, kind, field, embedding_model, embedding) \
                     VALUES (?1, ?2, 'concept', 'knowledge.atom', ?3, ?4)"
            ),
            params: vec![
                SqlValue::Text(subject.to_string()),
                SqlValue::Text(namespace.clone()),
                SqlValue::Text(WARM_TEST_MODEL.to_string()),
                SqlValue::Blob(bytes),
            ],
            label: None,
        })
        .await
        .expect("insert fresh vector");
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO ann_write_log \
                      (namespace, embedding_model, kind, field, subject_id, op) \
                      VALUES (?1, ?2, 'concept', 'knowledge.atom', ?3, 'upsert')"
                .into(),
            params: vec![
                SqlValue::Text(namespace),
                SqlValue::Text(WARM_TEST_MODEL.to_string()),
                SqlValue::Text(subject.to_string()),
            ],
            label: None,
        })
        .await
        .expect("append fresh-tail log row");
    subject
}

async fn append_warm_delete_tail(rt: &KhiveRuntime, count: usize) {
    let mut writer = rt.sql().writer().await.expect("writer");
    for _ in 0..count {
        writer
            .execute(SqlStatement {
                sql: "INSERT INTO ann_write_log \
                          (namespace, embedding_model, kind, field, subject_id, op) \
                          VALUES ('local', ?1, 'concept', 'knowledge.atom', ?2, 'delete')"
                    .into(),
                params: vec![
                    SqlValue::Text(WARM_TEST_MODEL.into()),
                    SqlValue::Text(Uuid::new_v4().to_string()),
                ],
                label: None,
            })
            .await
            .expect("append delete tail");
    }
}

#[tokio::test]
async fn classification_scope_counts_one_tail_row_uses_existence_bound() {
    let rt = memory_rt_with_embedder();
    let token = rt.authorize(Namespace::local()).expect("authorize");
    seed_warm_corpus_opts(&rt, &token, 12, false).await;
    append_warm_delete_tail(&rt, 1).await;

    let bounded = classification_scope_counts(&rt, "local", WARM_TEST_MODEL, 0, 0.20)
        .await
        .expect("bounded counts");
    assert_eq!(bounded.tail, 1);
    assert_eq!(bounded.live_lower_bound, 1);
    assert!(
        !bounded.live_count_exact,
        "one matching row proves Stale-tail"
    );
    assert_eq!(
        scope_counts(&rt, "local", WARM_TEST_MODEL, 0)
            .await
            .expect("exact counts"),
        (12, 1)
    );
}

#[tokio::test]
async fn classification_scope_counts_preserves_zero_and_threshold_boundary() {
    let empty_rt = memory_rt_with_embedder();
    let empty_token = empty_rt.authorize(Namespace::local()).expect("authorize");
    let _store = empty_rt
        .vectors_for_model(&empty_token, WARM_TEST_MODEL)
        .expect("create vec table");
    append_warm_delete_tail(&empty_rt, 1).await;
    let empty = classification_scope_counts(&empty_rt, "local", WARM_TEST_MODEL, 0, 0.20)
        .await
        .expect("empty counts");
    assert_eq!(empty.tail, 1);
    assert_eq!(empty.live_lower_bound, 0);
    assert!(
        empty.live_count_exact,
        "zero must be a predicate-scoped fact"
    );

    let rt = memory_rt_with_embedder();
    let token = rt.authorize(Namespace::local()).expect("authorize");
    seed_warm_corpus_opts(&rt, &token, 5, false).await;
    append_warm_delete_tail(&rt, 2).await;
    let below = classification_scope_counts(&rt, "local", WARM_TEST_MODEL, 0, 0.20)
        .await
        .expect("five-row counts");
    assert_eq!(below.tail, 2);
    assert_eq!(below.live_lower_bound, 5);
    assert!(below.live_count_exact);
    assert!(below.tail > (0.20 * below.live_lower_bound as f64).ceil() as u64);

    seed_warm_corpus_opts(&rt, &token, 1, false).await;
    let at = classification_scope_counts(&rt, "local", WARM_TEST_MODEL, 0, 0.20)
        .await
        .expect("six-row counts");
    assert_eq!(at.tail, 2);
    assert_eq!(at.live_lower_bound, 6);
    assert!(at.live_count_exact);
    assert!(at.tail <= (0.20 * at.live_lower_bound as f64).ceil() as u64);
}

#[test]
fn threshold_sized_tail_cap_scales_with_live_corpus() {
    let cap = |live: u64, threshold: f64| (live as f64 * threshold).ceil() as u64;
    assert_eq!(cap(3, 0.20), 1, "small corpora retain one newest row");
    assert_eq!(cap(5, 0.21), 2, "fractional limits round upward");
    assert_eq!(
        cap(1_000_000, 0.20),
        200_000,
        "large corpora must not collapse to the retired fixed 20k ceiling"
    );
}

#[tokio::test]
async fn capped_snapshot_selects_newest_threshold_sized_suffix() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt_with_embedder(dir.path().join("test.db"));
    let token = rt.authorize(Namespace::local()).expect("authorize");
    seed_warm_corpus(&rt, &token, 5).await;
    register_consumer(&rt, "local", WARM_TEST_MODEL)
        .await
        .expect("register");

    let mut reader = rt.sql().reader().await.expect("reader");
    let newest_rows = reader
        .query_all(SqlStatement {
            sql: "SELECT subject_id FROM ann_write_log \
                      WHERE namespace = 'local' AND embedding_model = ?1 \
                        AND field = 'knowledge.atom' \
                      ORDER BY seq DESC LIMIT 2"
                .into(),
            params: vec![SqlValue::Text(WARM_TEST_MODEL.into())],
            label: None,
        })
        .await
        .expect("latest log rows");
    let newest: Vec<Uuid> = newest_rows
        .iter()
        .map(|row| match row.get("subject_id") {
            Some(SqlValue::Text(subject)) => Uuid::parse_str(subject).expect("UUID"),
            other => panic!("unexpected subject: {other:?}"),
        })
        .collect();

    let one = fetch_fresh_tail_snapshot(&rt, "local", WARM_TEST_MODEL, 0, Some(0.20))
        .await
        .expect("20% snapshot");
    assert_eq!(one.live_count, Some(5));
    assert_eq!(one.ops.len(), 1, "ceil(5 × .20) = 1");
    assert_eq!(
        one.ops[0].0, newest[0],
        "the suffix must start newest-first"
    );

    let two = fetch_fresh_tail_snapshot(&rt, "local", WARM_TEST_MODEL, 0, Some(0.21))
        .await
        .expect("21% snapshot");
    assert_eq!(two.live_count, Some(5));
    assert_eq!(two.ops.len(), 2, "ceil(5 × .21) = 2");
    assert_eq!(
        two.ops
            .iter()
            .map(|(subject, _)| *subject)
            .collect::<Vec<_>>(),
        vec![newest[1], newest[0]],
        "selected newest rows are replayed in chronological order"
    );

    // A repeat op for the newest subject makes the two-row suffix contain
    // one distinct subject.  The final result must therefore coalesce to
    // one op, proving LIMIT is applied to raw newest writes before final-
    // state coalescing (the ADR-118 later-write boundary).
    let mut writer = rt.sql().writer().await.expect("writer");
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO ann_write_log \
                      (namespace, embedding_model, kind, field, subject_id, op) \
                      VALUES ('local', ?1, 'concept', 'knowledge.atom', ?2, 'upsert')"
                .into(),
            params: vec![
                SqlValue::Text(WARM_TEST_MODEL.into()),
                SqlValue::Text(newest[0].to_string()),
            ],
            label: None,
        })
        .await
        .expect("repeat newest write");
    drop(writer);
    let repeated = fetch_fresh_tail_snapshot(&rt, "local", WARM_TEST_MODEL, 0, Some(0.40))
        .await
        .expect("40% repeated snapshot");
    assert_eq!(
        repeated.ops.len(),
        1,
        "two newest writes coalesce to one subject"
    );
    assert_eq!(repeated.ops[0].0, newest[0]);
}

async fn append_warm_log_row(rt: &KhiveRuntime, subject: Uuid, op: &str) {
    let mut writer = rt.sql().writer().await.expect("writer");
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO ann_write_log \
                      (namespace, embedding_model, kind, field, subject_id, op) \
                      VALUES ('local', ?1, 'concept', 'knowledge.atom', ?2, ?3)"
                .into(),
            params: vec![
                SqlValue::Text(WARM_TEST_MODEL.into()),
                SqlValue::Text(subject.to_string()),
                SqlValue::Text(op.into()),
            ],
            label: None,
        })
        .await
        .expect("append log row");
}

/// A tail whose raw rows outnumber its subjects: the first subject is
/// written `repeats` more times and stays an upsert, the second ends as a
/// delete, the third is deleted and then upserted again, and `singles` more
/// subjects are written once. Returns every subject's embedding.
async fn seed_repeated_tail(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    repeats: usize,
    singles: usize,
) -> HashMap<Uuid, Vec<f32>> {
    let mut embeddings = HashMap::new();
    let mut subjects = Vec::new();
    for index in 0..3 + singles {
        let mut embedding = [0.0_f32; WARM_DIMS];
        embedding[index] = 1.0;
        let subject = append_warm_vector(rt, token, embedding).await;
        embeddings.insert(subject, embedding.to_vec());
        subjects.push(subject);
    }
    for _ in 0..repeats {
        append_warm_log_row(rt, subjects[0], "upsert").await;
    }
    append_warm_log_row(rt, subjects[1], "delete").await;
    append_warm_log_row(rt, subjects[2], "delete").await;
    append_warm_log_row(rt, subjects[2], "upsert").await;
    embeddings
}

/// Final states of the newest `limit` raw log rows (all rows when `None`),
/// coalesced by a plain pass over the rows in `seq` order: the position of
/// a subject's first row, the operation of its last.
async fn reference_tail_ops(
    rt: &KhiveRuntime,
    embeddings: &HashMap<Uuid, Vec<f32>>,
    limit: Option<usize>,
) -> Vec<(Uuid, Option<Vec<f32>>)> {
    let mut reader = rt.sql().reader().await.expect("reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT subject_id, op FROM ann_write_log \
                      WHERE namespace = 'local' AND embedding_model = ?1 \
                        AND field = 'knowledge.atom' \
                      ORDER BY seq DESC"
                .into(),
            params: vec![SqlValue::Text(WARM_TEST_MODEL.into())],
            label: None,
        })
        .await
        .expect("raw tail");
    let mut raw: Vec<(Uuid, bool)> = rows
        .iter()
        .take(limit.unwrap_or(usize::MAX))
        .map(|row| match (row.get("subject_id"), row.get("op")) {
            (Some(SqlValue::Text(subject)), Some(SqlValue::Text(op))) => {
                (Uuid::parse_str(subject).expect("UUID"), op == "delete")
            }
            other => panic!("unexpected log row: {other:?}"),
        })
        .collect();
    raw.reverse();

    let mut order = Vec::new();
    let mut last_is_delete = HashMap::new();
    for (subject, is_delete) in raw {
        if last_is_delete.insert(subject, is_delete).is_none() {
            order.push(subject);
        }
    }
    order
        .into_iter()
        .map(|subject| {
            let is_live = !last_is_delete[&subject];
            (subject, is_live.then(|| embeddings[&subject].clone()))
        })
        .collect()
}

#[tokio::test]
async fn fresh_tail_snapshot_statement_joins_one_row_per_distinct_subject() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt_with_embedder(dir.path().join("test.db"));
    let token = rt.authorize(Namespace::local()).expect("authorize");
    // 30 raw log rows over 7 subjects.
    let embeddings = seed_repeated_tail(&rt, &token, 20, 4).await;

    let mut reader = rt.sql().reader().await.expect("reader");
    let statement = fresh_tail_snapshot_statement("local", WARM_TEST_MODEL, 0, None);
    let rows = reader.query_all(statement).await.expect("snapshot rows");
    let tail_rows = rows
        .iter()
        .filter(|row| matches!(row.get("seq"), Some(SqlValue::Integer(_))))
        .count();
    let joined = rows
        .iter()
        .filter(|row| matches!(row.get("embedding"), Some(SqlValue::Blob(_))))
        .count();
    assert_eq!(
        tail_rows,
        embeddings.len(),
        "one tail row per distinct subject, not per raw log row"
    );
    assert_eq!(
        joined,
        embeddings.len(),
        "one embedding joined per distinct subject, not per raw log row"
    );
}

#[tokio::test]
async fn fresh_tail_snapshot_ops_match_per_row_coalescing_of_the_raw_log() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt_with_embedder(dir.path().join("test.db"));
    let token = rt.authorize(Namespace::local()).expect("authorize");
    let embeddings = seed_repeated_tail(&rt, &token, 20, 4).await;

    let full = fetch_fresh_tail_snapshot(&rt, "local", WARM_TEST_MODEL, 0, None)
        .await
        .expect("full snapshot");
    assert_eq!(full.ops, reference_tail_ops(&rt, &embeddings, None).await);

    // The cap keeps the newest raw rows before coalescing: 7 live vectors at
    // 0.5 keep ceil(3.5) = 4 rows, which are the last repeat of the first
    // subject, a delete, and a delete-then-upsert pair.
    let capped = fetch_fresh_tail_snapshot(&rt, "local", WARM_TEST_MODEL, 0, Some(0.5))
        .await
        .expect("capped snapshot");
    assert_eq!(capped.live_count, Some(7));
    assert_eq!(
        capped.ops,
        reference_tail_ops(&rt, &embeddings, Some(4)).await
    );
}

#[tokio::test]
async fn fresh_tail_snapshot_statement_probes_the_vector_table_by_point_lookup() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt_with_embedder(dir.path().join("test.db"));
    let token = rt.authorize(Namespace::local()).expect("authorize");
    seed_warm_corpus(&rt, &token, 1).await;

    let mut reader = rt.sql().reader().await.expect("reader");
    for threshold in [None, Some(0.2)] {
        let statement = fresh_tail_snapshot_statement("local", WARM_TEST_MODEL, 0, threshold);
        let rows = reader.explain(statement).await.expect("explain");
        // sqlite-vec reports idxStr '2!...' for its primary-key POINT plan and
        // '1' for a full scan. The capped form also scans the table once to
        // count live vectors, so only the join's plan may be a point plan.
        let mut point_plans = 0;
        for row in &rows {
            let Some(SqlValue::Text(detail)) = row.get("detail") else {
                continue;
            };
            if let Some((_, index)) = detail.rsplit_once(':') {
                if detail.contains("VIRTUAL TABLE INDEX ") && index.starts_with("2!") {
                    point_plans += 1;
                }
            }
        }
        assert_eq!(
            point_plans, 1,
            "the vector join must be one point lookup per tail subject: {rows:?}"
        );
    }
}

/// `ann_segment_dir` encodes a round-trippable hex key that `decode_ann_dir_name` reverses.
#[tokio::test]
async fn ann_segment_dir_encode_decode_round_trip() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt_with_embedder(dir.path().join("test.db"));
    let seg_dir = ann_segment_dir(&rt, "local", WARM_TEST_MODEL)
        .expect("file backend must return Some(seg_dir)");

    let dir_name = seg_dir
        .file_name()
        .expect("seg_dir must have a basename")
        .to_string_lossy()
        .into_owned();

    let (decoded_ns, decoded_model) =
        decode_ann_dir_name(&dir_name).expect("decode must succeed for a valid encode");
    assert_eq!(decoded_ns, "local");
    assert_eq!(decoded_model, WARM_TEST_MODEL);

    // Parent directory is the database's own ANN root (`<db-file>.ann/`
    // beside the file), so co-located databases never share segments.
    let parent = seg_dir.parent().expect("seg_dir must have a parent");
    assert_eq!(
        parent.file_name().unwrap().to_string_lossy(),
        "test.db.ann",
        "seg_dir parent must be the database-scoped ANN root"
    );
}

/// `ensure_ann_for_model` must not panic on an in-memory runtime (no data_dir).
#[tokio::test]
async fn ensure_ann_no_data_dir_does_not_panic() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let ann = new_shared();
    let token = rt.authorize(Namespace::local()).expect("authorize");
    // No data_dir → v2 path skipped. No corpus → no rebuild. Must complete silently.
    ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;
    let key = AnnKey::new("local", WARM_TEST_MODEL);
    assert!(
        !ann.indexes.read().await.contains_key(&key),
        "no index should be loaded when corpus is empty and model is unknown"
    );
}

/// Cold-start build persists v2 segments; a second call restores from disk.
///
/// Also gates the watermark contract: the persisted commit record must carry
/// `last_applied_seq` covering the seeded writes, so the second call's
/// classifier sees an empty tail and takes the Hot branch.
#[tokio::test]
async fn ensure_ann_round_trip_hot() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt_with_embedder(dir.path().join("test.db"));
    let token = rt.authorize(Namespace::local()).expect("authorize");
    seed_warm_corpus(&rt, &token, 4).await;

    // Cold-start: rebuild from corpus, persist v2 segments.
    let ann = new_shared();
    ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;
    let key = AnnKey::new("local", WARM_TEST_MODEL);
    assert!(
        ann.indexes.read().await.contains_key(&key),
        "first call must build the ANN index"
    );

    // Watermark contract: the extended commit record must carry a numeric
    // watermark covering every seeded write, so the tail above it is empty
    // and the Hot branch can fire.
    let seg_dir =
        ann_segment_dir(&rt, "local", WARM_TEST_MODEL).expect("file backend must have a seg_dir");
    assert!(
        seg_dir.join("metadata.bin").exists(),
        "first call must persist v2 segments (metadata.bin missing)"
    );
    let info = read_commit_info(&seg_dir)
        .expect("read_commit_info must not err")
        .expect("metadata.bin must carry a v2 commit record");
    let s = info
        .last_applied_seq
        .expect("checkpoint must persist an extended record with a watermark");
    let (live, tail) = scope_counts(&rt, "local", WARM_TEST_MODEL, s)
        .await
        .expect("scope_counts must succeed");
    assert!(live > 0, "seeded corpus must be live");
    assert_eq!(
        tail, 0,
        "watermark must cover every seeded write (empty tail)"
    );

    // Hot path: load from persisted v2 segments without rebuilding. A rebuild
    // would call save_atomic and rewrite metadata.bin (new inode); a true Hot
    // load via AnnBridge::load never writes. Asserting the inode is unchanged
    // proves the second call took the v2 Hot branch, not a silent rebuild.
    use std::os::unix::fs::MetadataExt;
    let meta_path = seg_dir.join("metadata.bin");
    let ino_before = std::fs::metadata(&meta_path)
        .expect("metadata.bin must exist after first build")
        .ino();
    let ann2 = new_shared();
    ensure_ann_for_model(&rt, &token, &ann2, WARM_TEST_MODEL).await;
    assert!(
        ann2.indexes.read().await.contains_key(&key),
        "second call must restore the ANN index from v2 segments"
    );
    let ino_after = std::fs::metadata(&meta_path)
        .expect("metadata.bin must still exist")
        .ino();
    assert_eq!(
        ino_before, ino_after,
        "second call must NOT rewrite metadata.bin — proves the v2 Hot load path, not a rebuild"
    );
}

#[tokio::test]
async fn knowledge_search_and_suggest_merge_write_above_loaded_bridge_watermark() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt_with_embedder(dir.path().join("test.db"));
    let token = rt.authorize(Namespace::local()).expect("authorize");
    seed_warm_corpus(&rt, &token, 4).await;

    let ann = new_shared();
    ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;
    let key = AnnKey::new("local", WARM_TEST_MODEL);
    let query = vec![1.0; WARM_DIMS];
    let (_, bridge_watermark) = search_loaded_with_seq(&ann, &key, &query, 20)
        .await
        .expect("serving bridge");

    let fresh_id = append_warm_vector(&rt, &token, [1.0; WARM_DIMS]).await;
    let now = chrono::Utc::now().timestamp_micros();
    let mut writer = rt.sql().writer().await.expect("writer");
    writer
            .execute(SqlStatement {
                sql: "INSERT INTO knowledge_atoms \
                      (id, namespace, slug, name, content, tags, properties, status, \
                       finalized, created_at, updated_at) \
                      VALUES (?1, 'local', 'opaque-fresh-tail', 'Opaque Fresh Tail', \
                              'content with no lexical overlap', '[\"type:domain\"]', '{}', 'reviewed', \
                              1, ?2, ?2)"
                    .into(),
                params: vec![SqlValue::Text(fresh_id.to_string()), SqlValue::Integer(now)],
                label: None,
            })
            .await
            .expect("insert hydratable knowledge atom");
    // The mirror atom above is what retrieval sees; the canonical row is what
    // member sizing measures, and suggest withholds a domain it cannot measure.
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO knowledge_domains \
                      (id, namespace, slug, name, description, members, created_at, updated_at) \
                      VALUES (?1, 'local', 'opaque-fresh-tail', 'Opaque Fresh Tail', \
                              'content with no lexical overlap', '[]', ?2, ?2)"
                .into(),
            params: vec![SqlValue::Text(fresh_id.to_string()), SqlValue::Integer(now)],
            label: None,
        })
        .await
        .expect("insert the canonical domain row the mirror atom stands for");
    drop(writer);

    let (_, still_loaded_watermark) = search_loaded_with_seq(&ann, &key, &query, 20)
        .await
        .expect("stale serving bridge remains loaded");
    assert_eq!(
        still_loaded_watermark, bridge_watermark,
        "the simulated external write must not mutate the in-process bridge"
    );

    let result = super::super::KnowledgeHandlers::search(
        &rt,
        &token,
        json!({
            "query": "quasar zephyr",
            "min_score": 0.1,
            "limit": 10,
            "rerank": false
        }),
        &ann,
    )
    .await
    .expect("knowledge.search");
    let ids: Vec<&str> = result["results"]
        .as_array()
        .expect("results")
        .iter()
        .filter_map(|hit| hit["id"].as_str())
        .collect();
    let fresh_id = fresh_id.to_string();
    assert!(
        ids.contains(&fresh_id.as_str()),
        "fresh-tail atom must be visible without rebuilding the loaded bridge: {result}"
    );

    let suggest = super::super::KnowledgeHandlers::suggest(
        &rt,
        &token,
        json!({
            "query": "quasar zephyr nebula pulsar aurora",
            "limit": 10
        }),
        &ann,
    )
    .await
    .expect("knowledge.suggest");
    let suggest_ids: Vec<&str> = suggest["results"]
        .as_array()
        .expect("suggest results")
        .iter()
        .filter_map(|hit| hit["id"].as_str())
        .collect();
    assert!(
        suggest_ids.contains(&fresh_id.as_str()),
        "fresh-tail domain atom must be visible to knowledge.suggest: {suggest}"
    );
}

#[tokio::test]
async fn fresh_tail_mismatch_replaces_stale_candidates_from_published_segment() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt_with_embedder(dir.path().join("test.db"));
    let token = rt.authorize(Namespace::local()).expect("authorize");
    seed_warm_corpus(&rt, &token, 4).await;

    let ann = new_shared();
    ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;
    let key = AnnKey::new("local", WARM_TEST_MODEL);
    let query = vec![1.0; WARM_DIMS];
    let (old_candidates, old_watermark) = search_loaded_with_seq(&ann, &key, &query, 20)
        .await
        .expect("old serving bridge");

    let fresh_id = append_warm_vector(&rt, &token, [1.0; WARM_DIMS]).await;
    assert!(
        !old_candidates
            .iter()
            .any(|(subject, _)| *subject == fresh_id),
        "the old bridge must not already contain the peer's fresh write"
    );

    // Simulate a peer checkpoint: publish a segment covering the fresh
    // write, raise the shared registry floor, and compact through it while
    // this process deliberately keeps its old bridge installed.
    let replacement = load_and_build_from_vector_store(&rt, &token, WARM_TEST_MODEL)
        .await
        .expect("scan replacement corpus")
        .expect("replacement corpus is non-empty");
    let replacement_watermark = replacement
        .index
        .last_applied_seq()
        .expect("replacement carries a watermark");
    assert!(replacement_watermark > old_watermark);
    persist_ann_v2(&rt, "local", WARM_TEST_MODEL, &replacement)
        .expect("publish replacement segment");
    raise_watermark(
        &rt,
        "local",
        WARM_TEST_MODEL,
        replacement_watermark,
        CheckpointAuthority::Incremental,
    )
    .await
    .expect("raise peer watermark");
    compact_log(&rt, "local", WARM_TEST_MODEL)
        .await
        .expect("compact through peer watermark");
    assert!(
        !tail_exists(&rt, "local", WARM_TEST_MODEL, old_watermark)
            .await
            .expect("probe compacted old tail"),
        "the old bridge's tail must be gone so only segment re-resolution can recover it"
    );

    let generation_before = current_generation(&ann, "local");
    let outcome = fresh_tail_leg(&rt, &ann, &key, &query, 20, Some(old_watermark)).await;
    let replacement_candidates = match outcome {
        FreshTailOutcome::Replace { candidates, .. } => candidates,
        FreshTailOutcome::Ops(_) => {
            panic!("a compaction mismatch must replace, not extend, stale candidates")
        }
        FreshTailOutcome::Skipped => panic!("mismatch re-resolution must not disappear"),
    };

    assert!(
        replacement_candidates
            .iter()
            .any(|(subject, _)| *subject == fresh_id),
        "current-query re-resolution must surface the write covered by the published segment"
    );
    assert!(
        current_generation(&ann, "local") > generation_before,
        "mismatch handling must retire the stale cache generation"
    );
    assert!(
        search_loaded_with_seq(&ann, &key, &query, 20)
            .await
            .is_none(),
        "the stale in-process bridge must be evicted so normal warming re-adopts the segment"
    );
}

#[tokio::test]
async fn registry_loss_evicts_stale_bridge_and_forces_authoritative_rebuild() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt_with_embedder(dir.path().join("test.db"));
    let token = rt.authorize(Namespace::local()).expect("authorize");
    seed_warm_corpus(&rt, &token, 4).await;

    let ann = new_shared();
    ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;
    let key = AnnKey::new("local", WARM_TEST_MODEL);
    let query = vec![1.0; WARM_DIMS];
    let (_, stale_watermark) = search_loaded_with_seq(&ann, &key, &query, 20)
        .await
        .expect("serving bridge");
    let fresh_id = append_warm_vector(&rt, &token, [1.0; WARM_DIMS]).await;

    // Simulate administrative loss followed by a peer consumer compacting
    // the interval this process never checkpointed.  The stale bridge can
    // no longer be repaired from its `> S` tail.
    let mut writer = rt.sql().writer().await.expect("writer");
    writer
        .execute(SqlStatement {
            sql: "DELETE FROM ann_consumer_watermark \
                      WHERE consumer = ?1 AND namespace = 'local' \
                        AND embedding_model = ?2"
                .into(),
            params: vec![
                SqlValue::Text(ANN_CONSUMER.into()),
                SqlValue::Text(WARM_TEST_MODEL.into()),
            ],
            label: None,
        })
        .await
        .expect("delete own registry row");
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO ann_consumer_watermark \
                      (consumer, namespace, embedding_model, watermark) \
                      VALUES ('peer:test', 'local', ?1, 999)"
                .into(),
            params: vec![SqlValue::Text(WARM_TEST_MODEL.into())],
            label: None,
        })
        .await
        .expect("insert peer watermark");
    drop(writer);
    compact_log(&rt, "local", WARM_TEST_MODEL)
        .await
        .expect("compact missing interval");
    assert!(
        !tail_exists(&rt, "local", WARM_TEST_MODEL, stale_watermark)
            .await
            .expect("tail probe"),
        "setup must remove the stale bridge's recovery tail"
    );

    let outcome = fresh_tail_leg(&rt, &ann, &key, &query, 20, Some(stale_watermark)).await;
    assert!(
        matches!(
            outcome,
            FreshTailOutcome::Replace { ref candidates, source_exhausted: true }
                if candidates.is_empty()
        ),
        "the query that detects registry loss must discard stale candidates"
    );
    assert!(search_loaded_with_seq(&ann, &key, &query, 20)
        .await
        .is_none());
    assert_eq!(
        read_own_watermark(&rt, "local", WARM_TEST_MODEL)
            .await
            .expect("registry read"),
        Some(-1),
        "the cross-process sentinel must stay durable through the Cold transition"
    );

    assert_eq!(
        ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await,
        AnnWarmOutcome::Ready
    );
    let rebuilt = search_loaded(&ann, &key, &query, 20)
        .await
        .expect("authoritative rebuilt bridge");
    assert!(
        rebuilt.iter().any(|(subject, _)| *subject == fresh_id),
        "the next warm must rebuild from the full corpus, not re-adopt the stale segment"
    );
    assert!(
        read_own_watermark(&rt, "local", WARM_TEST_MODEL)
            .await
            .expect("registry read")
            .is_some_and(|watermark| watermark >= 0),
        "successful authoritative publication must clear the sentinel"
    );
    assert!(!force_rebuild_required(&ann, &key));
}

/// After a corpus mutation the persisted segment has a non-empty tail and the
/// classifier replays it (Stale-tail), re-persisting a checkpoint that
/// reflects the mutated corpus.
#[tokio::test]
async fn ensure_ann_stale_rebuild() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt_with_embedder(dir.path().join("test.db"));
    let token = rt.authorize(Namespace::local()).expect("authorize");
    seed_warm_corpus(&rt, &token, 4).await;

    // Initial build: persist v2 segments.
    let ann = new_shared();
    ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;
    let key = AnnKey::new("local", WARM_TEST_MODEL);
    assert!(ann.indexes.read().await.contains_key(&key), "initial build");

    // Mutate corpus: add one more row.
    seed_warm_corpus(&rt, &token, 1).await;

    // Gate: the mutation's logged write must sit above the persisted
    // watermark — the Stale-tail pre-condition.
    let seg_dir =
        ann_segment_dir(&rt, "local", WARM_TEST_MODEL).expect("file backend must have a seg_dir");
    let info_before = read_commit_info(&seg_dir)
        .expect("read_commit_info must not err")
        .expect("v2 commit record must be present after initial build");
    let s_before = info_before
        .last_applied_seq
        .expect("initial checkpoint must carry a watermark");
    let (_, tail) = scope_counts(&rt, "local", WARM_TEST_MODEL, s_before)
        .await
        .expect("scope_counts must succeed");
    assert!(
        tail > 0,
        "mutation must appear as a tail row above the watermark"
    );

    // Fresh SharedAnn: non-empty tail detected → replay + checkpoint.
    let ann2 = new_shared();
    ensure_ann_for_model(&rt, &token, &ann2, WARM_TEST_MODEL).await;
    assert!(
        ann2.indexes.read().await.contains_key(&key),
        "must serve an index after corpus mutation (tail replayed)"
    );

    // The post-replay checkpoint must reflect the mutated (5-row) corpus
    // and advance the watermark past the mutation's log row.
    let info_after = read_commit_info(&seg_dir)
        .expect("read_commit_info after replay must not err")
        .expect("v2 commit record must be present after replay checkpoint");
    assert_eq!(
        info_after.vector_count, 5,
        "checkpoint must reflect the 5-row corpus (4 initial + 1 replayed)"
    );
    let s_after = info_after
        .last_applied_seq
        .expect("replay checkpoint must carry a watermark");
    assert!(s_after > s_before, "checkpoint must advance the watermark");
    let (_, tail_after) = scope_counts(&rt, "local", WARM_TEST_MODEL, s_after)
        .await
        .expect("scope_counts must succeed");
    assert_eq!(
        tail_after, 0,
        "replayed tail must be covered by the new watermark"
    );
}

/// Review-mandated case: a checkpoint taken over an EMPTY log persists the
/// zero watermark (the defined empty-log baseline), and the first logged
/// write afterwards classifies Stale-tail — never Hot.
#[tokio::test]
async fn ensure_ann_zero_watermark_then_first_write_is_stale_tail() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt_with_embedder(dir.path().join("test.db"));
    let token = rt.authorize(Namespace::local()).expect("authorize");
    // Corpus WITHOUT log rows: the log is empty at checkpoint time.
    seed_warm_corpus_opts(&rt, &token, 4, false).await;

    let ann = new_shared();
    ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;
    let seg_dir = ann_segment_dir(&rt, "local", WARM_TEST_MODEL).expect("seg_dir");
    let info = read_commit_info(&seg_dir)
        .expect("read_commit_info")
        .expect("v2 commit record");
    assert_eq!(
        info.last_applied_seq,
        Some(0),
        "empty-log checkpoint must persist the numeric zero baseline, not a missing watermark"
    );

    // First logged write after the zero-watermark checkpoint.
    seed_warm_corpus_opts(&rt, &token, 1, true).await;
    let ann2 = new_shared();
    ensure_ann_for_model(&rt, &token, &ann2, WARM_TEST_MODEL).await;
    let key = AnnKey::new("local", WARM_TEST_MODEL);
    let n = ann2
        .indexes
        .read()
        .await
        .get(&key)
        .map(AnnBridge::num_vectors)
        .expect("index must be served after the first logged write");
    assert_eq!(
        n, 5,
        "Stale-tail must replay the logged write (Hot would serve 4)"
    );
    let info2 = read_commit_info(&seg_dir)
        .expect("read_commit_info")
        .expect("v2 commit record after replay");
    assert!(
        info2.last_applied_seq.unwrap_or(0) > 0,
        "replay checkpoint must advance past the zero baseline"
    );
}

/// Review-mandated case: deleting every live vector classifies Empty — no
/// ANN is served or replayed, and the terminal unavailable marker is set.
#[tokio::test]
async fn ensure_ann_delete_all_is_empty() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt_with_embedder(dir.path().join("test.db"));
    let token = rt.authorize(Namespace::local()).expect("authorize");
    seed_warm_corpus(&rt, &token, 3).await;

    let ann = new_shared();
    ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;
    let key = AnnKey::new("local", WARM_TEST_MODEL);
    assert!(ann.indexes.read().await.contains_key(&key), "initial build");

    // Delete every corpus row, logging each delete (production funnel shape).
    let table = format!("vec_{}", sanitize_model_key(WARM_TEST_MODEL));
    let sql = rt.sql();
    let mut w = sql.writer().await.expect("writer");
    w.execute(SqlStatement {
        sql: format!(
            "INSERT INTO ann_write_log \
                 (namespace, embedding_model, kind, field, subject_id, op) \
                 SELECT namespace, embedding_model, kind, field, subject_id, 'delete' \
                 FROM {table} WHERE namespace = ?1 AND embedding_model = ?2"
        ),
        params: vec![
            SqlValue::Text("local".into()),
            SqlValue::Text(WARM_TEST_MODEL.into()),
        ],
        label: None,
    })
    .await
    .expect("log deletes");
    w.execute(SqlStatement {
        sql: format!("DELETE FROM {table} WHERE namespace = ?1 AND embedding_model = ?2"),
        params: vec![
            SqlValue::Text("local".into()),
            SqlValue::Text(WARM_TEST_MODEL.into()),
        ],
        label: None,
    })
    .await
    .expect("delete corpus");
    drop(w);

    let ann2 = new_shared();
    ensure_ann_for_model(&rt, &token, &ann2, WARM_TEST_MODEL).await;
    assert!(
        !ann2.indexes.read().await.contains_key(&key),
        "zero live corpus must classify Empty — no ANN served (rule 5 precedes Hot)"
    );
    assert!(
        is_terminally_unavailable(&ann2, &key),
        "Empty must set the terminal unavailable marker for wait_ready"
    );
}

/// Review-mandated interleaving: consumer A registers pending, checkpoints
/// at S, and crashes before its raise — the pair MIN stays negative and another
/// consumer's compaction cannot delete A's tail. After A's row is raised
/// (or an overlapping row removed), compaction advances to the pair MIN.
#[tokio::test]
async fn compact_log_bounded_by_pair_minimum() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt_with_embedder(dir.path().join("test.db"));
    let token = rt.authorize(Namespace::local()).expect("authorize");
    seed_warm_corpus(&rt, &token, 4).await; // seqs 1..=4 in the log

    register_consumer(&rt, "local", WARM_TEST_MODEL)
        .await
        .expect("register pending");
    // Overlapping consumer B durably checkpointed past every row.
    {
        let sql = rt.sql();
        let mut w = sql.writer().await.expect("writer");
        w.execute(SqlStatement {
            sql: "INSERT INTO ann_consumer_watermark \
                      (consumer, namespace, embedding_model, watermark) VALUES (?1, ?2, ?3, 99)"
                .into(),
            params: vec![
                SqlValue::Text("other:test".into()),
                SqlValue::Text("local".into()),
                SqlValue::Text(WARM_TEST_MODEL.into()),
            ],
            label: None,
        })
        .await
        .expect("insert B row");
    }

    // A crashed before its raise: row is -2 → MIN(-2, 99) = -2 → nothing deletes.
    compact_log(&rt, "local", WARM_TEST_MODEL)
        .await
        .expect("compact");
    let (_, tail_while_pending) = scope_counts(&rt, "local", WARM_TEST_MODEL, 0)
        .await
        .expect("scope_counts");
    assert_eq!(
        tail_while_pending, 4,
        "a fresh pending row must block pair compaction"
    );

    // A raises to 2 → MIN(2, 99) = 2 → rows 1-2 compact, 3-4 remain.
    raise_watermark(
        &rt,
        "local",
        WARM_TEST_MODEL,
        2,
        CheckpointAuthority::FullRegistered,
    )
    .await
    .expect("raise");
    compact_log(&rt, "local", WARM_TEST_MODEL)
        .await
        .expect("compact");
    let (_, tail_after) = scope_counts(&rt, "local", WARM_TEST_MODEL, 0)
        .await
        .expect("scope_counts");
    assert_eq!(
        tail_after, 2,
        "compaction must advance exactly to the pair MIN"
    );
}

/// #1479 regression: a consumer which never published its first
/// checkpoint blocks during its grace window, then retires visibly so an
/// overlapping active consumer's watermark can bound compaction.
#[tokio::test]
async fn compact_log_retires_expired_never_activated_consumer() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt_with_embedder(dir.path().join("test.db"));
    let token = rt.authorize(Namespace::local()).expect("authorize");
    seed_warm_corpus(&rt, &token, 4).await;

    register_consumer(&rt, "local", WARM_TEST_MODEL)
        .await
        .expect("register pending");
    let sql = rt.sql();
    let mut writer = sql.writer().await.expect("writer");
    writer
        .execute(SqlStatement {
            sql: "UPDATE ann_consumer_pending SET registered_at_us = 1 \
                      WHERE consumer = ?1 AND namespace = 'local' \
                        AND embedding_model = ?2"
                .into(),
            params: vec![
                SqlValue::Text(ANN_CONSUMER.into()),
                SqlValue::Text(WARM_TEST_MODEL.into()),
            ],
            label: Some("test_age_pending_ann_consumer".into()),
        })
        .await
        .expect("age pending registration");
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO ann_consumer_watermark \
                      (consumer, namespace, embedding_model, watermark) \
                      VALUES ('other:test', 'local', ?1, 99)"
                .into(),
            params: vec![SqlValue::Text(WARM_TEST_MODEL.into())],
            label: None,
        })
        .await
        .expect("insert active peer");
    drop(writer);

    compact_log(&rt, "local", WARM_TEST_MODEL)
        .await
        .expect("retire and compact");
    assert_eq!(
        read_own_watermark(&rt, "local", WARM_TEST_MODEL)
            .await
            .expect("read retired registration"),
        None,
        "expired pending consumer must be removed so its return is Cold"
    );
    let (_, retained) = scope_counts(&rt, "local", WARM_TEST_MODEL, 0)
        .await
        .expect("scope counts");
    assert_eq!(
        retained, 0,
        "the retired pending row must no longer pin the active peer's minimum"
    );
}

#[tokio::test]
async fn ordinary_checkpoint_cannot_clear_force_rebuild_sentinel() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt_with_embedder(dir.path().join("test.db"));
    let ann = new_shared();
    let key = AnnKey::new("local", WARM_TEST_MODEL);
    prepare_authoritative_rebuild(&rt, &ann, &key)
        .await
        .expect("publish sentinel");

    assert!(
        raise_watermark(
            &rt,
            "local",
            WARM_TEST_MODEL,
            7,
            CheckpointAuthority::Incremental,
        )
        .await
        .is_err(),
        "an ordinary checkpoint must lose the conditional publication fence"
    );
    assert_eq!(
        read_own_watermark(&rt, "local", WARM_TEST_MODEL)
            .await
            .expect("read sentinel"),
        Some(-1)
    );

    raise_watermark(
        &rt,
        "local",
        WARM_TEST_MODEL,
        7,
        CheckpointAuthority::FullSentinel,
    )
    .await
    .expect("authoritative checkpoint clears sentinel");
    assert_eq!(
        read_own_watermark(&rt, "local", WARM_TEST_MODEL)
            .await
            .expect("read raised watermark"),
        Some(7)
    );
}

#[tokio::test]
async fn full_scan_checkpoint_clears_local_force_rebuild_marker() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt_with_embedder(dir.path().join("test.db"));
    let token = rt.authorize(Namespace::local()).expect("authorize");
    seed_warm_corpus(&rt, &token, 4).await;

    let ann = new_shared();
    let key = AnnKey::new("local", WARM_TEST_MODEL);
    register_consumer(&rt, "local", WARM_TEST_MODEL)
        .await
        .expect("register pending consumer");
    assert_eq!(
        read_own_watermark(&rt, "local", WARM_TEST_MODEL)
            .await
            .expect("read pending watermark"),
        Some(ann_registry::PENDING_WATERMARK)
    );
    let authority = prepare_full_corpus_scan(&rt, &ann, &key)
        .await
        .expect("establish full-scan authority");
    assert_eq!(authority, CheckpointAuthority::FullSentinel);
    assert_eq!(
        read_own_watermark(&rt, "local", WARM_TEST_MODEL)
            .await
            .expect("read recovery fence"),
        Some(ann_registry::RECOVERING_WATERMARK),
        "a direct full scan must promote pending to the authoritative fence before scanning"
    );
    assert!(force_rebuild_required(&ann, &key));

    let bridge = load_and_build_from_vector_store(&rt, &token, WARM_TEST_MODEL)
        .await
        .expect("scan corpus")
        .expect("non-empty corpus");
    assert!(
        checkpoint_raise_compact_readopt(
            &rt,
            &ann,
            &key,
            bridge,
            current_generation(&ann, "local"),
            authority,
        )
        .await,
        "authoritative full scan must publish"
    );
    assert!(
        !force_rebuild_required(&ann, &key),
        "the shared checkpoint seam must finish local recovery for direct rebuild callers"
    );

    let query = vec![1.0; WARM_DIMS];
    let (_, watermark) = search_loaded_with_seq(&ann, &key, &query, 20)
        .await
        .expect("published bridge");
    assert!(
        matches!(
            fresh_tail_leg(&rt, &ann, &key, &query, 20, Some(watermark)).await,
            FreshTailOutcome::Ops(_)
        ),
        "the next query must retain the freshly published bridge"
    );
}

#[tokio::test]
async fn concurrent_full_sentinel_loser_does_not_restore_sentinel() {
    let rt = memory_rt_with_embedder();
    let token = rt.authorize(Namespace::local()).expect("authorize");
    seed_warm_corpus(&rt, &token, 4).await;

    let ann = new_shared();
    let key = AnnKey::new("local", WARM_TEST_MODEL);
    let authority_a = prepare_full_corpus_scan(&rt, &ann, &key)
        .await
        .expect("first full-scan authority");
    let authority_b = prepare_full_corpus_scan(&rt, &ann, &key)
        .await
        .expect("second full-scan authority");
    assert_eq!(authority_a, CheckpointAuthority::FullSentinel);
    assert_eq!(authority_b, CheckpointAuthority::FullSentinel);

    let bridge_a = load_and_build_from_vector_store(&rt, &token, WARM_TEST_MODEL)
        .await
        .expect("scan A")
        .expect("non-empty A");
    let bridge_b = load_and_build_from_vector_store(&rt, &token, WARM_TEST_MODEL)
        .await
        .expect("scan B")
        .expect("non-empty B");
    let generation = current_generation(&ann, "local");
    let (published_a, published_b) = tokio::join!(
        checkpoint_raise_compact_readopt(&rt, &ann, &key, bridge_a, generation, authority_a,),
        checkpoint_raise_compact_readopt(&rt, &ann, &key, bridge_b, generation, authority_b,)
    );

    assert!(published_a || published_b, "one full scan must win");
    assert!(
        read_own_watermark(&rt, "local", WARM_TEST_MODEL)
            .await
            .expect("read winner watermark")
            .is_some_and(|watermark| watermark >= 0),
        "the losing checkpoint must not demote the winner back to -1"
    );
    assert!(!force_rebuild_required(&ann, &key));
    assert!(
        has_current_index(&ann, &key).await,
        "the winner must remain available after the losing fence"
    );
}

#[tokio::test]
async fn concurrent_pathless_normal_checkpoints_publish_monotonically() {
    let rt = memory_rt_with_embedder();
    let token = rt.authorize(Namespace::local()).expect("authorize");
    seed_warm_corpus(&rt, &token, 4).await;
    let ann = new_shared();
    assert_eq!(
        ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await,
        AnnWarmOutcome::Ready
    );

    let key = AnnKey::new("local", WARM_TEST_MODEL);
    let old_authority = prepare_full_corpus_scan(&rt, &ann, &key)
        .await
        .expect("old authority");
    let old_bridge = load_and_build_from_vector_store(&rt, &token, WARM_TEST_MODEL)
        .await
        .expect("old scan")
        .expect("old corpus");
    let old_watermark = old_bridge.index.last_applied_seq().unwrap_or(0);

    let fresh_id = append_warm_vector(&rt, &token, [1.0; WARM_DIMS]).await;
    let new_authority = prepare_full_corpus_scan(&rt, &ann, &key)
        .await
        .expect("new authority");
    let new_bridge = load_and_build_from_vector_store(&rt, &token, WARM_TEST_MODEL)
        .await
        .expect("new scan")
        .expect("new corpus");
    let new_watermark = new_bridge.index.last_applied_seq().unwrap_or(0);
    assert!(new_watermark > old_watermark);

    let generation = current_generation(&ann, "local");
    let (old_result, new_result) = tokio::join!(
        checkpoint_raise_compact_readopt(&rt, &ann, &key, old_bridge, generation, old_authority,),
        checkpoint_raise_compact_readopt(&rt, &ann, &key, new_bridge, generation, new_authority,)
    );

    assert!(old_result && new_result);
    assert_eq!(
        read_own_watermark(&rt, "local", WARM_TEST_MODEL)
            .await
            .expect("registry read"),
        Some(i64::try_from(new_watermark).expect("watermark range"))
    );
    let query = vec![1.0; WARM_DIMS];
    let (hits, loaded_watermark) = search_loaded_with_seq(&ann, &key, &query, 20)
        .await
        .expect("monotone winner");
    assert_eq!(loaded_watermark, new_watermark);
    assert!(hits.iter().any(|(subject, _)| *subject == fresh_id));
}

/// In-memory SqlBridge writers share one connection. Lifecycle helpers
/// must therefore remain single statements on pathless runtimes instead
/// of trying to open a nested manual atomic-unit transaction.
#[tokio::test]
async fn pathless_lifecycle_helpers_do_not_open_nested_transactions() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

    let rt = memory_rt_with_embedder();
    let key = AnnKey::new("local", WARM_TEST_MODEL);
    // A pooled writer rolls back a transaction still open when it is
    // returned, so no transaction can be held across helper calls. Deny
    // transaction control on the shared connection instead: a BEGIN,
    // COMMIT, ROLLBACK or SAVEPOINT issued by any helper fails.
    let set_tripwire = |armed: bool| {
        let writer = rt.backend().pool().writer().expect("pathless writer");
        let result = if armed {
            writer.authorizer(Some(|context: AuthContext<'_>| match context.action {
                AuthAction::Transaction { .. } | AuthAction::Savepoint { .. } => {
                    Authorization::Deny
                }
                _ => Authorization::Allow,
            }))
        } else {
            writer.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        };
        result.expect("set the transaction tripwire");
    };
    set_tripwire(true);
    // The probe goes to the pooled connection itself: `SqlWriter::execute`
    // refuses transaction control before SQLite sees it, so a probe through
    // it would pass without the authorizer.
    let probe = rt
        .backend()
        .pool()
        .writer()
        .expect("probe writer")
        .execute_batch("BEGIN IMMEDIATE");
    assert!(
        probe.is_err(),
        "the tripwire must refuse transaction control: {probe:?}"
    );

    register_consumer(&rt, "local", WARM_TEST_MODEL)
        .await
        .expect("register pending without a transaction");
    write_force_rebuild_sentinel_row(&rt, &key)
        .await
        .expect("publish recovery sentinel without a transaction");
    raise_watermark(
        &rt,
        "local",
        WARM_TEST_MODEL,
        0,
        CheckpointAuthority::FullSentinel,
    )
    .await
    .expect("activate sentinel without a transaction");
    compact_log(&rt, "local", WARM_TEST_MODEL)
        .await
        .expect("compact without a transaction");
    assert_eq!(
        read_own_watermark(&rt, "local", WARM_TEST_MODEL)
            .await
            .expect("read pathless lifecycle state"),
        Some(0)
    );
    set_tripwire(false);
}

#[tokio::test]
async fn stale_normal_checkpoint_adopts_newer_publisher_without_overwrite() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt_with_embedder(dir.path().join("test.db"));
    let token = rt.authorize(Namespace::local()).expect("authorize");
    seed_warm_corpus(&rt, &token, 4).await;

    let initial_ann = new_shared();
    assert_eq!(
        ensure_ann_for_model(&rt, &token, &initial_ann, WARM_TEST_MODEL).await,
        AnnWarmOutcome::Ready
    );
    let key = AnnKey::new("local", WARM_TEST_MODEL);
    let stale_authority = prepare_full_corpus_scan(&rt, &initial_ann, &key)
        .await
        .expect("stale authority");
    let stale_bridge = load_and_build_from_vector_store(&rt, &token, WARM_TEST_MODEL)
        .await
        .expect("stale scan")
        .expect("stale corpus");
    let stale_watermark = stale_bridge.index.last_applied_seq().unwrap_or(0);

    let fresh_id = append_warm_vector(&rt, &token, [1.0; WARM_DIMS]).await;
    let winner_ann = new_shared();
    let winner_authority = prepare_full_corpus_scan(&rt, &winner_ann, &key)
        .await
        .expect("winner authority");
    let winner_bridge = load_and_build_from_vector_store(&rt, &token, WARM_TEST_MODEL)
        .await
        .expect("winner scan")
        .expect("winner corpus");
    let winner_watermark = winner_bridge.index.last_applied_seq().unwrap_or(0);
    assert!(winner_watermark > stale_watermark);
    assert!(
        checkpoint_raise_compact_readopt(
            &rt,
            &winner_ann,
            &key,
            winner_bridge,
            current_generation(&winner_ann, "local"),
            winner_authority,
        )
        .await,
        "newer publisher must commit"
    );

    let stale_ann = new_shared();
    assert!(
        checkpoint_raise_compact_readopt(
            &rt,
            &stale_ann,
            &key,
            stale_bridge,
            current_generation(&stale_ann, "local"),
            stale_authority,
        )
        .await,
        "stale publisher must adopt the winner instead of overwriting it"
    );
    assert_eq!(
        read_own_watermark(&rt, "local", WARM_TEST_MODEL)
            .await
            .expect("registry read"),
        Some(i64::try_from(winner_watermark).expect("watermark range"))
    );
    let info = read_commit_info(
        &ann_segment_dir(&rt, "local", WARM_TEST_MODEL).expect("segment directory"),
    )
    .expect("commit read")
    .expect("commit info");
    assert_eq!(info.last_applied_seq, Some(winner_watermark));
    assert_eq!(info.vector_count, 5);
    let query = vec![1.0; WARM_DIMS];
    assert!(
        search_loaded(&stale_ann, &key, &query, 20)
            .await
            .expect("adopted winner")
            .iter()
            .any(|(subject, _)| *subject == fresh_id),
        "the stale publisher must serve the winner's fresh row"
    );
}

#[tokio::test]
async fn first_use_empty_corpus_retains_cross_process_sentinel() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt_with_embedder(dir.path().join("test.db"));
    let token = rt.authorize(Namespace::local()).expect("authorize");
    let ann = new_shared();
    let key = AnnKey::new("local", WARM_TEST_MODEL);

    assert_eq!(
        ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await,
        AnnWarmOutcome::Empty
    );
    assert_eq!(
        read_own_watermark(&rt, "local", WARM_TEST_MODEL)
            .await
            .expect("registry read"),
        Some(-1),
        "even a fresh process must preserve the only cross-process loss signal"
    );
    assert!(force_rebuild_required(&ann, &key));
}

#[tokio::test]
async fn delayed_sentinel_request_cannot_demote_completed_recovery() {
    let rt = memory_rt_with_embedder();
    let token = rt.authorize(Namespace::local()).expect("authorize");
    seed_warm_corpus(&rt, &token, 4).await;
    let ann = new_shared();
    let key = AnnKey::new("local", WARM_TEST_MODEL);
    assert_eq!(
        ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await,
        AnnWarmOutcome::Ready
    );
    let winner = read_own_watermark(&rt, "local", WARM_TEST_MODEL)
        .await
        .expect("winner watermark")
        .expect("registered winner");
    assert!(winner >= 0);

    prepare_authoritative_rebuild(&rt, &ann, &key)
        .await
        .expect("delayed sentinel request");
    assert_eq!(
        read_own_watermark(&rt, "local", WARM_TEST_MODEL)
            .await
            .expect("registry read"),
        Some(winner),
        "revalidation under the publication lock must preserve the winner"
    );
    assert!(
        force_rebuild_required(&ann, &key),
        "the delayed detector must remain Cold until it scans or adopts authoritatively"
    );
}

/// Review-mandated case: a pre-amendment commit record (base length, no
/// watermark trailer) classifies Cold and rebuilds — never serves Hot.
#[tokio::test]
async fn ensure_ann_pre_amendment_record_is_cold() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt_with_embedder(dir.path().join("test.db"));
    let token = rt.authorize(Namespace::local()).expect("authorize");
    seed_warm_corpus(&rt, &token, 4).await;

    let ann = new_shared();
    ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;
    let seg_dir = ann_segment_dir(&rt, "local", WARM_TEST_MODEL).expect("seg_dir");

    // Truncate the 41-byte watermark/codes trailer plus the 16-byte
    // publication nonce: the record parses at the base length — a
    // legitimate pre-amendment commit with no watermark.
    let meta_path = seg_dir.join("metadata.bin");
    let bytes = std::fs::read(&meta_path).expect("read metadata.bin");
    std::fs::write(&meta_path, &bytes[..bytes.len() - 57]).expect("truncate trailer");
    let info = read_commit_info(&seg_dir)
        .expect("read_commit_info")
        .expect("base-length record must still parse");
    assert_eq!(
        info.last_applied_seq, None,
        "trailer removed → no watermark"
    );

    use std::os::unix::fs::MetadataExt;
    let ino_before = std::fs::metadata(&meta_path).expect("meta").ino();
    let ann2 = new_shared();
    ensure_ann_for_model(&rt, &token, &ann2, WARM_TEST_MODEL).await;
    let key = AnnKey::new("local", WARM_TEST_MODEL);
    assert!(
        ann2.indexes.read().await.contains_key(&key),
        "Cold rebuild must still produce a served index"
    );
    let ino_after = std::fs::metadata(&meta_path).expect("meta").ino();
    assert_ne!(
        ino_before, ino_after,
        "pre-amendment record must force a rebuild (metadata.bin rewritten), not a Hot load"
    );
    let info2 = read_commit_info(&seg_dir)
        .expect("read_commit_info")
        .expect("rebuilt record");
    assert!(
        info2.last_applied_seq.is_some(),
        "rebuild must restore the extended watermark record"
    );
}

/// `ensure_ann_for_model`'s fast path must treat a present-but-generation-stale
/// cached entry as a miss, not a hit (PR #815). In production
/// `install_if_fresher`'s own fencing prevents a stale entry from ever
/// installing, so this test bumps the namespace generation directly
/// (bypassing `clear_namespace`'s eviction) to construct the "present but
/// stale" state as an independent, defense-in-depth check on the fast path
/// itself — mere presence must never again be trusted as freshness.
#[tokio::test]
async fn ensure_ann_fast_path_ignores_generation_stale_cached_entry() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt_with_embedder(dir.path().join("test.db"));
    let token = rt.authorize(Namespace::local()).expect("authorize");
    seed_warm_corpus(&rt, &token, 4).await;

    let ann = new_shared();
    ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;
    let key = AnnKey::new("local", WARM_TEST_MODEL);
    assert!(
        ann.indexes.read().await.contains_key(&key),
        "setup: first call must build and install the index at generation 0"
    );

    // Bump the namespace's generation directly, leaving the generation-0
    // entry present — the state install_if_fresher's fencing prevents in
    // production, exercised here purely to isolate the fast-path check.
    bump_generation(&ann, "local");
    assert_eq!(current_generation(&ann, "local"), 1);

    ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;

    // If the in-memory fast path had (incorrectly) treated mere presence
    // as a hit, it would return immediately and the cached entry's
    // generation would still read 0. Falling through re-stamps it with
    // the namespace's current generation (1) via the v2/rebuild paths —
    // proof the stale entry was NOT served as a hit.
    assert_eq!(
        ann.indexes
            .read()
            .await
            .get(&key)
            .expect("entry present")
            .generation,
        1,
        "a present-but-generation-stale entry must not short-circuit via the fast \
             path; the reloaded/rebuilt entry must be re-stamped with the namespace's \
             new current generation"
    );
}

/// `warm_known_snapshots` must warm v2 segments even when the legacy
/// `retrieval_snapshots` table is absent (the v1 query errors). Pre-fix it
/// early-returned on that error and never reached the filesystem segment
/// enumeration, so v2-only databases never warmed at daemon startup.
#[tokio::test]
async fn warm_known_snapshots_v2_only_no_legacy_table() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt_with_embedder(dir.path().join("test.db"));
    let token = rt.authorize(Namespace::local()).expect("authorize");
    seed_warm_corpus(&rt, &token, 4).await;

    // Setup: build + persist v2 segments to data_dir/ann/<hex>/.
    let ann = new_shared();
    ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;
    let key = AnnKey::new("local", WARM_TEST_MODEL);
    assert!(
        ann.indexes.read().await.contains_key(&key),
        "setup: first ensure must persist v2 segments"
    );

    // Force the worst case the fix targets: the v1 table is absent, so the
    // legacy query errors. Pre-fix, that error aborted the whole warm pass.
    {
        let sql = rt.sql();
        let mut w = sql.writer().await.expect("writer");
        w.execute(SqlStatement {
            sql: "DROP TABLE IF EXISTS retrieval_snapshots".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("drop retrieval_snapshots");
    }

    // Cold cache + warm: the v2 filesystem enumeration must still warm the
    // key despite the v1 query error.
    let ann_fresh = new_shared();
    warm_known_snapshots(&rt, &ann_fresh).await;
    assert!(
        ann_fresh.indexes.read().await.contains_key(&key),
        "warm_known_snapshots must warm v2 segments when retrieval_snapshots is absent \
             (regression: a v1 query error must not abort the v2 filesystem pass)"
    );
}

/// End-to-end reproduction of issue #1026: an empty corpus must leave the
/// key marked unavailable so `wait_ready` short-circuits instead of
/// polling out the full warm-wait timeout on every query.
#[tokio::test]
async fn ensure_ann_for_model_empty_corpus_marks_unavailable_and_wait_short_circuits() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt_with_embedder(dir.path().join("test.db"));
    let token = rt.authorize(Namespace::local()).expect("authorize");
    // No seed_warm_corpus call — the corpus stays empty for this model.

    let ann = new_shared();
    ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;
    let key = AnnKey::new("local", WARM_TEST_MODEL);
    assert!(
        !ann.indexes.read().await.contains_key(&key),
        "empty corpus must not install an index"
    );

    let start = std::time::Instant::now();
    let ready = wait_ready(&ann, &key, ANN_WARM_WAIT_TIMEOUT_MS, ANN_WARM_WAIT_POLL_MS).await;
    let elapsed = start.elapsed();

    assert!(!ready, "empty corpus must never become ready");
    assert_terminal_wait_latency(elapsed);
}

/// A rebuild error is operational, not proof of an unbuildable corpus:
/// it must NOT leave an unavailable marker, so the retry the background
/// path arranges (by removing the warming key) still gets a bounded wait
/// instead of an instant `false` (issue #1026).
#[tokio::test]
async fn ensure_ann_for_model_rebuild_error_does_not_mark_unavailable() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt_with_embedder(dir.path().join("test.db"));
    let token = rt.authorize(Namespace::local()).expect("authorize");
    seed_warm_corpus(&rt, &token, 3).await;

    // Swap the corpus table for a view over a missing table so any scan
    // query fails operationally (SQLite validates views at query time).
    let model_key = sanitize_model_key(WARM_TEST_MODEL);
    let table = format!("vec_{model_key}");
    let sql = rt.sql();
    let mut w = sql.writer().await.expect("writer");
    w.execute(SqlStatement {
        sql: format!("DROP TABLE {table}"),
        params: vec![],
        label: None,
    })
    .await
    .expect("drop corpus table");
    w.execute(SqlStatement {
        sql: format!("CREATE VIEW {table} AS SELECT * FROM missing_corpus_table"),
        params: vec![],
        label: None,
    })
    .await
    .expect("create broken view");
    drop(w);

    let ann = new_shared();
    ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;
    let key = AnnKey::new("local", WARM_TEST_MODEL);

    assert!(
        !ann.indexes.read().await.contains_key(&key),
        "a failed rebuild must not install an index"
    );
    assert!(
        !unavailable_guard(&ann.unavailable).contains_key(&key),
        "a rebuild ERROR must not mark the key unavailable — only a completed \
             empty-corpus scan may; a marker here would short-circuit wait_ready \
             while the same-generation retry is in flight"
    );

    // The next request's wait must still observe an index installed
    // mid-poll by the same-generation retry.
    let ann2 = ann.clone();
    let key2 = key.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        let bridge = AnnBridge::build(vec![1.0f32, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
            .expect("build")
            .with_generation(0);
        install_if_fresher(&ann2, &key2, bridge).await;
    });
    let ready = wait_ready(&ann, &key, 500, 10).await;
    assert!(
        ready,
        "after a rebuild error the wait must keep polling and observe the \
             retry's install, not short-circuit false"
    );
}

/// A process that is not the warm index host must not build the corpus, and —
/// the part that matters on a shared index root — must not publish a segment or
/// claim the rebuild in the registry on its way to declining. Claiming without
/// building would fence every other consumer behind a rebuild nobody is doing.
#[tokio::test]
async fn a_non_warm_host_declines_the_corpus_build_and_writes_nothing() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt_with_embedder(dir.path().join("test.db"));
    let token = rt.authorize(Namespace::local()).expect("authorize");
    seed_warm_corpus(&rt, &token, 3).await;

    let ann = new_shared_for_role(false);
    let outcome = ensure_ann_for_model(&rt, &token, &ann, WARM_TEST_MODEL).await;
    assert_eq!(outcome, AnnWarmOutcome::Declined);

    let key = AnnKey::new("local", WARM_TEST_MODEL);
    assert!(
        !ann.indexes.read().await.contains_key(&key),
        "a declined warm must not install an index"
    );
    if let Some(seg_dir) = ann_segment_dir(&rt, "local", WARM_TEST_MODEL) {
        assert!(
            !seg_dir.join("metadata.bin").exists(),
            "a declined warm must not publish a segment"
        );
    }
    assert!(
        matches!(
            read_own_watermark(&rt, "local", WARM_TEST_MODEL).await,
            Ok(None)
        ),
        "a declined warm must leave the registry untouched: no row, and in \
             particular no rebuild sentinel this process will never satisfy"
    );
}

/// A store-opening failure (here: a model with no registered embedder)
/// must propagate as an error, not collapse into `Ok(None)` — otherwise
/// it would be indistinguishable from a verified empty corpus and leave
/// a terminal unavailable marker that blocks the same-generation retry.
#[tokio::test]
async fn ensure_ann_for_model_store_open_failure_does_not_mark_unavailable() {
    let dir = TempDir::new().expect("tempdir");
    let rt = file_rt_with_embedder(dir.path().join("test.db"));
    let token = rt.authorize(Namespace::local()).expect("authorize");

    let model = "model-with-no-registered-embedder";
    assert!(
        rt.vectors_for_model(&token, model).is_err(),
        "precondition: opening the vector store for an unregistered model must fail"
    );

    let ann = new_shared();
    ensure_ann_for_model(&rt, &token, &ann, model).await;
    let key = AnnKey::new("local", model);

    assert!(
        !unavailable_guard(&ann.unavailable).contains_key(&key),
        "a store-opening failure must not mark the key unavailable"
    );

    // The next request's wait must still observe a same-generation install.
    let ann2 = ann.clone();
    let key2 = key.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        let bridge = AnnBridge::build(vec![1.0f32, 0.0, 0.0, 0.0], 4, vec![Uuid::new_v4()])
            .expect("build")
            .with_generation(0);
        install_if_fresher(&ann2, &key2, bridge).await;
    });
    let ready = wait_ready(&ann, &key, 500, 10).await;
    assert!(
        ready,
        "after a store-opening failure the wait must keep polling and observe \
             the retry's install, not short-circuit false"
    );
}

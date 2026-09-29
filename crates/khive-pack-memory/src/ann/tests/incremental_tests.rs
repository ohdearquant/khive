use super::*;
use std::time::Duration;

const DIMS: usize = 8;
const SEED_COUNT: usize = 80;

fn threshold_policy(ann: &SharedAnn) {
    *ann.checkpoint_policy.write().expect("checkpoint policy") = CheckpointPolicy {
        max_dirty_ops: 4,
        interval: Duration::ZERO,
        consolidate_tau: 40_000,
        rebuild_fraction: 0.20,
    };
}

#[tokio::test]
async fn protected_tail_materializes_only_one_row_beyond_replay_cap() {
    const MODEL: &str = "ann-bounded-incremental-tail-model";
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    rt.register_embedder(crate::test_support::HashVecProvider {
        model_name: MODEL.to_owned(),
        dims: DIMS,
    });
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    for index in 0..12 {
        write_note(&rt, &token, &format!("bounded tail note {index}")).await;
    }

    let mut reader = rt.sql().reader().await.expect("sql reader");
    let (rebuild, observed) = fetch_protected_tail_on(reader.as_mut(), MODEL, 0, 2)
        .await
        .expect("bounded protected tail");
    assert!(rebuild.is_none(), "the raw suffix exceeds the replay cap");
    assert_eq!(
        observed, 3,
        "the materialized CTE needs only cap + 1 rows to require a rebuild"
    );

    let (within_cap, observed) = fetch_protected_tail_on(reader.as_mut(), MODEL, 0, 12)
        .await
        .expect("within-cap protected tail");
    let (ops, _end, raw_count) = within_cap.expect("all twelve rows fit the replay cap");
    assert_eq!(observed, 12);
    assert_eq!(raw_count, 12);
    assert_eq!(ops.len(), 12);
}

async fn write_note(rt: &KhiveRuntime, token: &NamespaceToken, text: &str) -> Uuid {
    rt.create_note_with_decay_for_embedding_model(
        token,
        "memory",
        None,
        text,
        Some(0.7),
        0.01,
        None,
        vec![],
        None,
    )
    .await
    .expect("write note with its real hash embedding")
    .id
}

async fn seeded(model: &str) -> (TestRuntime, NamespaceToken, SharedAnn, AnnKey, Vec<Uuid>) {
    let rt = test_runtime_with_hash_embedder(model, DIMS);
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    let mut ids = Vec::new();
    for i in 0..SEED_COUNT {
        ids.push(write_note(&rt, &token, &format!("incremental seed note {i}")).await);
    }
    let ann = new_shared();
    threshold_policy(&ann);
    let key = AnnKey::new(model);
    let status = ensure_ann_for_model(&rt, &token, &ann, model)
        .await
        .expect("initial warm");
    assert!(
        matches!(
            status,
            AnnEnsureStatus::Built {
                vectors: SEED_COUNT
            }
        ),
        "initial corpus must build all seeded vectors: {status:?}"
    );
    (rt, token, ann, key, ids)
}

fn commit_record(rt: &KhiveRuntime, model: &str) -> Vec<u8> {
    let dir = ann_segment_dir(rt, model).expect("file-backed segment directory");
    std::fs::read(dir.join("metadata.bin")).expect("read segment commit record")
}

async fn completions(rt: &KhiveRuntime, token: &NamespaceToken) -> Vec<khive_storage::Event> {
    rt.events(token)
        .expect("event store")
        .query_events(
            khive_storage::EventFilter {
                verbs: vec!["memory.ann_warm".into()],
                kinds: vec![khive_types::EventKind::PhaseCompleted],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                limit: 128,
                offset: 0,
            },
        )
        .await
        .expect("query warm completions")
        .items
}

async fn warm_with_event(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    ann: &SharedAnn,
    model: &str,
) -> (AnnEnsureStatus, serde_json::Value) {
    let before: HashSet<_> = completions(rt, token)
        .await
        .into_iter()
        .map(|event| event.id)
        .collect();
    let status = ensure_ann_for_model(rt, token, ann, model)
        .await
        .expect("warm must succeed");
    let mut added: Vec<_> = completions(rt, token)
        .await
        .into_iter()
        .filter(|event| !before.contains(&event.id))
        .collect();
    assert_eq!(
        added.len(),
        1,
        "each maintenance attempt emits one completion"
    );
    (status, added.pop().expect("one completion").payload)
}

async fn assert_recalled(
    rt: &KhiveRuntime,
    ann: &SharedAnn,
    key: &AnnKey,
    model: &str,
    id: Uuid,
    text: &str,
) {
    let query = fnv_to_vec(text, DIMS);
    let (raw, seq) = search_loaded_with_seq(ann, key, &query, SEED_COUNT + 8)
        .await
        .expect("search installed bridge")
        .expect("bridge remains installed");
    let outcome = fresh_tail_leg(rt, ann, key, model, &query, SEED_COUNT + 8, Some(seq)).await;
    let (merged, reason) = outcome_into_candidates(outcome, raw, &query);
    assert_eq!(reason, None, "incremental recall must not degrade");
    assert!(
        merged
            .iter()
            .any(|(hit, score)| *hit == id && *score > 0.99),
        "merged recall must return the current vector for {id}: {merged:?}"
    );
}

#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn below_threshold_warms_keep_segment_and_serve_each_write() {
    const MODEL: &str = "ann-incremental-below-threshold-model";
    let (rt, token, ann, key, _) = seeded(MODEL).await;
    let committed = commit_record(&rt, MODEL);
    let loads = ann.segment_load_count.load(Ordering::SeqCst);
    let publications = ann.publication_count.load(Ordering::SeqCst);
    let published_seq = bridge_applied_seq(&ann, &key)
        .await
        .expect("seed watermark");
    let mut written = Vec::new();

    for i in 0..3 {
        let text = format!("unpublished incremental note {i}");
        let id = write_note(&rt, &token, &text).await;
        bump_generation(&ann, &key).await;
        // Read-your-writes must hold even before the queued warm runs.
        assert_recalled(&rt, &ann, &key, MODEL, id, &text).await;
        let (_, event) = warm_with_event(&rt, &token, &ann, MODEL).await;
        assert_eq!(
            ann.segment_load_count.load(Ordering::SeqCst),
            loads,
            "INCREMENTAL_NO_SEGMENT_LOAD: below-threshold warm must retain the installed bridge"
        );
        assert_eq!(
            ann.publication_count.load(Ordering::SeqCst),
            publications,
            "INCREMENTAL_NO_PUBLICATION: below-threshold warm must leave the checkpoint alone"
        );
        assert_eq!(
            commit_record(&rt, MODEL),
            committed,
            "INCREMENTAL_COMMIT_UNCHANGED: unpublished writes must not rotate metadata.bin"
        );
        assert_eq!(event["path"], "incremental_in_place");
        assert_eq!(event["ops_applied"], 1);
        assert!(is_current(&ann, &key).await, "applied bridge must be fresh");
        let query = fnv_to_vec(&text, DIMS);
        let (_, recall_seq) = search_loaded_with_seq(&ann, &key, &query, 1)
            .await
            .expect("search dirty bridge")
            .expect("dirty bridge installed");
        assert_eq!(
            recall_seq, published_seq,
            "INCREMENTAL_RECALL_FLOOR: unpublished inserts must retain exact-tail coverage"
        );
        assert!(
            bridge_applied_seq(&ann, &key)
                .await
                .expect("applied watermark")
                > recall_seq,
            "live replay must advance beyond the persisted recall floor"
        );
        written.push((id, text));
        for (id, text) in &written {
            assert_recalled(&rt, &ann, &key, MODEL, *id, text).await;
        }
    }
}

#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn dirty_threshold_publishes_once_and_fresh_state_adopts_hot() {
    const MODEL: &str = "ann-incremental-checkpoint-model";
    let (rt, token, ann, key, _) = seeded(MODEL).await;
    let committed = commit_record(&rt, MODEL);
    let publications = ann.publication_count.load(Ordering::SeqCst);
    let mut written = Vec::new();
    for i in 0..4 {
        let text = format!("checkpoint threshold note {i}");
        let id = write_note(&rt, &token, &text).await;
        bump_generation(&ann, &key).await;
        let (_, event) = warm_with_event(&rt, &token, &ann, MODEL).await;
        let expected = u64::from(i == 3);
        assert_eq!(
            ann.publication_count.load(Ordering::SeqCst) - publications,
            expected as usize,
            "INCREMENTAL_THRESHOLD_ONCE: the fourth dirty operation must cause exactly one publication"
        );
        assert_eq!(
            event["path"],
            if i == 3 {
                "incremental_checkpoint"
            } else {
                "incremental_in_place"
            }
        );
        assert_eq!(event["ops_applied"], 1);
        written.push((id, text));
    }
    assert_eq!(
        commit_record(&rt, MODEL),
        committed,
        "a delta checkpoint must preserve the base segment nonce"
    );
    assert!(
        ann_segment_dir(&rt, MODEL)
            .expect("segment directory")
            .join(delta::HEAD_FILE)
            .exists(),
        "the due checkpoint must publish a delta beside the base segment"
    );

    let restarted = new_shared();
    threshold_policy(&restarted);
    let (status, event) = warm_with_event(&rt, &token, &restarted, MODEL).await;
    assert!(
        matches!(status, AnnEnsureStatus::LoadedSnapshot),
        "HOT_RESTART_NO_BUILD: {status:?}"
    );
    assert_eq!(
        event["path"], "segment_load",
        "HOT_RESTART_PATH: checkpoint must leave no replay tail"
    );
    assert_eq!(event["ops_applied"], 0);
    assert_eq!(restarted.segment_load_count.load(Ordering::SeqCst), 1);
    assert_eq!(restarted.publication_count.load(Ordering::SeqCst), 0);
    for (id, text) in written {
        assert_recalled(&rt, &restarted, &key, MODEL, id, &text).await;
    }
}

#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn failed_full_checkpoint_readopt_can_publish_next_incremental_delta() {
    const MODEL: &str = "ann-full-checkpoint-readopt-fallback-model";
    let rt = test_runtime_with_hash_embedder(MODEL, DIMS);
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    for i in 0..SEED_COUNT {
        write_note(&rt, &token, &format!("readopt fallback seed {i}")).await;
    }
    let ann = new_shared();
    *ann.checkpoint_policy.write().expect("checkpoint policy") = CheckpointPolicy {
        max_dirty_ops: 1,
        interval: Duration::ZERO,
        consolidate_tau: 40_000,
        rebuild_fraction: 0.20,
    };
    let key = AnnKey::new(MODEL);
    let dir = ann_segment_dir(&rt, MODEL).expect("file-backed segment directory");

    // Control: a bridge without a committed base is not allowed to append a
    // delta. The fallback below must acquire a real digest from its full save.
    let mut no_base = AnnBridge::build(vec![1.0; DIMS], DIMS, vec![Uuid::new_v4()], HashSet::new())
        .expect("build no-base control");
    no_base.record_delta_batch(vec![(Uuid::new_v4(), None)], 1, 1);
    assert_eq!(
        delta::write(&dir, &no_base)
            .err()
            .expect("missing base must fail"),
        "memory delta has no base segment commit",
        "a missing base digest must refuse delta publication"
    );

    ann.fail_next_segment_load.store(true, Ordering::SeqCst);
    let initial = ensure_ann_for_model(&rt, &token, &ann, MODEL)
        .await
        .expect("full checkpoint must install fallback");
    assert!(matches!(
        initial,
        AnnEnsureStatus::Built {
            vectors: SEED_COUNT
        }
    ));
    assert_eq!(ann.segment_load_count.load(Ordering::SeqCst), 1);
    assert!(
        !ann.fail_next_segment_load.load(Ordering::SeqCst),
        "the forced re-adoption failure must have been consumed"
    );
    let digest = segment_commit_digest(&dir)
        .expect("read full checkpoint digest")
        .expect("full checkpoint exists");
    let loaded = AnnBridge::load(&dir).expect("saved base is valid without injected failure");
    assert_eq!(loaded.commit_digest, Some(digest));
    {
        let indexes = ann.indexes.read().await;
        let fallback = indexes.get(&key).expect("owned fallback installed");
        assert_eq!(fallback.base_commit_digest, Some(digest));
        assert_eq!(fallback.commit_digest, Some(digest));
        assert_eq!(
            fallback.base_applied_seq,
            fallback.index.last_applied_seq().unwrap()
        );
        assert_eq!(fallback.base_ops, SEED_COUNT);
        assert_eq!(fallback.delta_raw_ops, 0);
        assert!(fallback.last_delta_nonce.is_none());
    }

    let text = "incremental write after failed mmap re-adoption";
    let id = write_note(&rt, &token, text).await;
    bump_generation(&ann, &key).await;
    let (_, event) = warm_with_event(&rt, &token, &ann, MODEL).await;
    assert_eq!(event["path"], "incremental_checkpoint");
    assert!(dir.join(delta::HEAD_FILE).exists());
    assert_recalled(&rt, &ann, &key, MODEL, id, text).await;
    let restarted = AnnBridge::load(&dir).expect("delta based on saved checkpoint must load");
    assert_eq!(restarted.base_commit_digest, Some(digest));
    assert!(restarted.id_map.contains(&id));
}

#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn failed_installed_compaction_readopt_resets_base_for_next_delta() {
    const MODEL: &str = "ann-installed-compaction-readopt-fallback-model";
    let (rt, token, ann, key, _) = seeded(MODEL).await;
    let dir = ann_segment_dir(&rt, MODEL).expect("segment directory");
    {
        let mut indexes = ann.indexes.write().await;
        let bridge = indexes.get_mut(&key).expect("seeded bridge");
        bridge.delta_raw_ops = delta::compaction_limit(bridge.base_ops) - 1;
    }
    ann.fail_next_segment_load.store(true, Ordering::SeqCst);
    write_note(&rt, &token, "write crossing compaction limit").await;
    bump_generation(&ann, &key).await;
    let (_, event) = warm_with_event(&rt, &token, &ann, MODEL).await;
    assert_eq!(event["path"], "incremental_checkpoint");
    assert!(!ann.fail_next_segment_load.load(Ordering::SeqCst));
    let digest = segment_commit_digest(&dir)
        .expect("read compacted commit")
        .expect("compacted commit exists");
    {
        let indexes = ann.indexes.read().await;
        let fallback = indexes.get(&key).expect("installed owned fallback");
        assert_eq!(fallback.base_commit_digest, Some(digest));
        assert_eq!(fallback.commit_digest, Some(digest));
        assert_eq!(fallback.delta_raw_ops, 0);
        assert!(fallback.delta_batches.is_empty());
        assert!(fallback.last_delta_nonce.is_none());
    }

    ann.checkpoint_policy
        .write()
        .expect("checkpoint policy")
        .max_dirty_ops = 1;
    let id = write_note(&rt, &token, "write following failed compaction readopt").await;
    bump_generation(&ann, &key).await;
    let (_, event) = warm_with_event(&rt, &token, &ann, MODEL).await;
    assert_eq!(event["path"], "incremental_checkpoint");
    assert!(dir.join(delta::HEAD_FILE).exists());
    let reopened = AnnBridge::load(&dir).expect("delta on compacted base must load");
    assert_eq!(reopened.base_commit_digest, Some(digest));
    assert!(reopened.id_map.contains(&id));
}

#[tokio::test]
#[serial(pathless_fresh_tail)]
async fn pathless_incremental_checkpoint_keeps_committed_rows_visible_during_publication() {
    const MODEL: &str = "ann-pathless-incremental-checkpoint-race-model";
    const SEED_COUNT: usize = 8;
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    rt.register_embedder(crate::test_support::HashVecProvider {
        model_name: MODEL.to_owned(),
        dims: DIMS,
    });
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    for i in 0..SEED_COUNT {
        write_note(&rt, &token, &format!("pathless checkpoint seed {i}")).await;
    }

    let ann = new_shared();
    *ann.checkpoint_policy.write().expect("checkpoint policy") = CheckpointPolicy {
        max_dirty_ops: 2,
        interval: Duration::ZERO,
        consolidate_tau: 40_000,
        rebuild_fraction: 0.20,
    };
    let key = AnnKey::new(MODEL);
    let initial = ensure_ann_for_model(&rt, &token, &ann, MODEL)
        .await
        .expect("initial pathless warm");
    assert!(matches!(
        initial,
        AnnEnsureStatus::Built {
            vectors: SEED_COUNT
        }
    ));
    assert!(
        ann_segment_dir(&rt, MODEL).is_none(),
        "test must use pathless publication"
    );
    let baseline = read_own_watermark(&rt, MODEL)
        .await
        .expect("read initial watermark")
        .and_then(|watermark| u64::try_from(watermark).ok())
        .expect("active initial watermark");

    let committed = [
        "pathless checkpoint committed row alpha",
        "pathless checkpoint committed row beta",
    ]
    .map(str::to_owned);
    let mut committed_ids = Vec::with_capacity(committed.len());
    for text in &committed {
        committed_ids.push(write_note(&rt, &token, text).await);
        bump_generation(&ann, &key).await;
    }

    ann.pathless_checkpoint_barrier
        .store(true, Ordering::SeqCst);
    let task_rt = rt.clone();
    let task_token = token.clone();
    let task_ann = ann.clone();
    let checkpoint =
        tokio::spawn(
            async move { ensure_ann_for_model(&task_rt, &task_token, &task_ann, MODEL).await },
        );
    tokio::time::timeout(
        Duration::from_secs(10),
        ann.pathless_checkpoint_notify.notified(),
    )
    .await
    .expect("checkpoint must pause before pathless watermark publication");

    assert_eq!(
        read_own_watermark(&rt, MODEL)
            .await
            .expect("read paused watermark"),
        Some(baseline as i64),
        "the active registry floor must remain unchanged while the exact tail is needed"
    );
    let query = fnv_to_vec(&committed[0], DIMS);
    let (raw, seq) = search_loaded_with_seq(&ann, &key, &query, 1)
        .await
        .expect("search paused bridge")
        .expect("bridge remains installed");
    let outcome = fresh_tail_leg(&rt, &ann, &key, MODEL, &query, 1, Some(seq)).await;
    let (merged, reason) = outcome_into_candidates(outcome, raw, &query);
    assert_eq!(
        reason, None,
        "paused recall must retain a healthy exact leg"
    );
    let returned: HashSet<_> = merged.into_iter().map(|(id, _)| id).collect();
    assert!(
        committed_ids.iter().all(|id| returned.contains(id)),
        "every committed row must be returned while the checkpoint is paused: {returned:?}"
    );
    assert_eq!(
        seq, baseline,
        "the dirty bridge must retain the prior exact-tail floor until publication"
    );
    assert_eq!(
        bridge_applied_seq(&ann, &key).await,
        Some(baseline + committed_ids.len() as u64),
        "the paused bridge must already contain both committed rows"
    );

    ann.pathless_checkpoint_release.notify_one();
    let status = checkpoint
        .await
        .expect("checkpoint task must finish")
        .expect("pathless incremental checkpoint must succeed");
    assert!(matches!(status, AnnEnsureStatus::AlreadyLoaded));
    assert_eq!(
        read_own_watermark(&rt, MODEL)
            .await
            .expect("read published watermark"),
        Some((baseline + committed_ids.len() as u64) as i64)
    );
    let indexes = ann.indexes.read().await;
    let bridge = indexes.get(&key).expect("published pathless bridge");
    assert!(bridge.delta_batches.is_empty());
    assert_eq!(bridge.delta_raw_ops, 0);
}

#[tokio::test]
#[serial(pathless_fresh_tail)]
async fn pathless_checkpoint_writer_wait_leaves_index_available_to_snapshot_reader() {
    const MODEL: &str = "ann-pathless-checkpoint-lock-order-model";
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    rt.register_embedder(crate::test_support::HashVecProvider {
        model_name: MODEL.to_owned(),
        dims: DIMS,
    });
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    for index in 0..8 {
        write_note(&rt, &token, &format!("lock order seed {index}")).await;
    }
    let ann = new_shared();
    *ann.checkpoint_policy.write().expect("checkpoint policy") = CheckpointPolicy {
        max_dirty_ops: 1,
        interval: Duration::ZERO,
        consolidate_tau: 40_000,
        rebuild_fraction: 0.20,
    };
    let key = AnnKey::new(MODEL);
    ensure_ann_for_model(&rt, &token, &ann, MODEL)
        .await
        .expect("initial pathless warm");
    assert!(ann_segment_dir(&rt, MODEL).is_none());

    let fresh_text = "lock order committed note";
    let fresh_id = write_note(&rt, &token, fresh_text).await;
    bump_generation(&ann, &key).await;
    ann.pathless_checkpoint_barrier
        .store(true, Ordering::SeqCst);
    let task_rt = rt.clone();
    let task_token = token.clone();
    let task_ann = ann.clone();
    let checkpoint =
        tokio::spawn(
            async move { ensure_ann_for_model(&task_rt, &task_token, &task_ann, MODEL).await },
        );
    tokio::time::timeout(
        Duration::from_secs(10),
        ann.pathless_checkpoint_notify.notified(),
    )
    .await
    .expect("checkpoint must pause before watermark publication");

    let mut reader = rt.sql().reader().await.expect("pathless reader");
    begin_read_snapshot(reader.as_mut())
        .await
        .expect("pin pathless read snapshot");
    registry_min_watermark_on(reader.as_mut(), MODEL)
        .await
        .expect("read pinned registry floor");
    ann.pathless_checkpoint_release.notify_one();
    tokio::time::timeout(
        Duration::from_secs(10),
        ann.pathless_watermark_attempt_notify.notified(),
    )
    .await
    .expect("checkpoint must reach its SQL writer attempt");

    let indexes = tokio::time::timeout(Duration::from_secs(1), ann.indexes.read())
        .await
        .expect("snapshot reader must acquire index lock while SQL writer waits");
    assert!(
        indexes.contains_key(&key),
        "the warm bridge stays installed"
    );
    drop(indexes);
    end_read_snapshot(reader.as_mut()).await;
    drop(reader);

    let status = tokio::time::timeout(Duration::from_secs(10), checkpoint)
        .await
        .expect("checkpoint completes after the read snapshot closes")
        .expect("checkpoint task")
        .expect("pathless checkpoint succeeds without evicting the bridge");
    assert!(matches!(status, AnnEnsureStatus::AlreadyLoaded));
    let query = fnv_to_vec(fresh_text, DIMS);
    let (raw, seq) = search_loaded_with_seq(&ann, &key, &query, 20)
        .await
        .expect("search retained bridge")
        .expect("warm bridge remains installed");
    let outcome = fresh_tail_leg(&rt, &ann, &key, MODEL, &query, 20, Some(seq)).await;
    let (merged, _) = outcome_into_candidates(outcome, raw, &query);
    assert!(
        merged.iter().any(|(id, _)| *id == fresh_id),
        "committed memory remains in ANN candidates after checkpoint contention"
    );
}

#[tokio::test]
#[serial(pathless_fresh_tail)]
async fn pathless_mismatch_waits_for_checkpoint_without_dropping_committed_candidate() {
    const MODEL: &str = "ann-pathless-post-watermark-mismatch-model";
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    rt.register_embedder(crate::test_support::HashVecProvider {
        model_name: MODEL.to_owned(),
        dims: DIMS,
    });
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    for index in 0..8 {
        write_note(&rt, &token, &format!("post-watermark seed {index}")).await;
    }
    let ann = new_shared();
    *ann.checkpoint_policy.write().expect("checkpoint policy") = CheckpointPolicy {
        max_dirty_ops: 1,
        interval: Duration::ZERO,
        consolidate_tau: 40_000,
        rebuild_fraction: 0.20,
    };
    let key = AnnKey::new(MODEL);
    ensure_ann_for_model(&rt, &token, &ann, MODEL)
        .await
        .expect("initial pathless warm");
    assert!(ann_segment_dir(&rt, MODEL).is_none());

    let fresh_text = "post-watermark committed note";
    let fresh_id = write_note(&rt, &token, fresh_text).await;
    bump_generation(&ann, &key).await;
    let query = fnv_to_vec(fresh_text, DIMS);
    let (stale_raw, stale_s) = search_loaded_with_seq(&ann, &key, &query, 20)
        .await
        .expect("search incumbent bridge")
        .expect("incumbent bridge remains installed");
    ann.pathless_post_watermark_barrier
        .store(true, Ordering::SeqCst);
    let task_rt = rt.clone();
    let task_token = token.clone();
    let task_ann = ann.clone();
    let checkpoint =
        tokio::spawn(
            async move { ensure_ann_for_model(&task_rt, &task_token, &task_ann, MODEL).await },
        );
    tokio::time::timeout(
        Duration::from_secs(10),
        ann.pathless_post_watermark_notify.notified(),
    )
    .await
    .expect("checkpoint must pause after SQL watermark publication");
    let raised_floor = read_own_watermark(&rt, MODEL)
        .await
        .expect("read raised watermark")
        .expect("active watermark");
    assert!(raised_floor > stale_s as i64);
    let (_, still_old_s) = search_loaded_with_seq(&ann, &key, &query, 20)
        .await
        .expect("search dirty bridge")
        .expect("bridge remains installed");
    assert_eq!(still_old_s, stale_s, "bridge publication is still pending");

    let recall_rt = rt.clone();
    let recall_ann = ann.clone();
    let recall_key = key.clone();
    let recall_query = query.clone();
    let recall = tokio::spawn(async move {
        fresh_tail_leg(
            &recall_rt,
            &recall_ann,
            &recall_key,
            MODEL,
            &recall_query,
            20,
            Some(stale_s),
        )
        .await
    });
    tokio::time::timeout(
        Duration::from_secs(10),
        ann.pathless_reresolve_wait_notify.notified(),
    )
    .await
    .expect("mismatch recovery must release its SQL snapshot before waiting");
    ann.pathless_post_watermark_release.notify_one();
    tokio::time::timeout(Duration::from_secs(10), checkpoint)
        .await
        .expect("checkpoint must finish without a pinned reader")
        .expect("checkpoint task")
        .expect("pathless checkpoint succeeds");
    let outcome = tokio::time::timeout(Duration::from_secs(10), recall)
        .await
        .expect("mismatch recovery finishes after checkpoint publication")
        .expect("recall task");
    let (merged, reason) = outcome_into_candidates(outcome, stale_raw, &query);
    assert_eq!(reason, None, "re-resolved exact leg remains healthy");
    assert!(
        merged.iter().any(|(id, _)| *id == fresh_id),
        "the committed note survives the transient floor/bridge mismatch"
    );
}

#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn unpublished_tail_restarts_by_replay_without_full_build() {
    const MODEL: &str = "ann-incremental-unpublished-restart-model";
    let (rt, token, ann, key, seed_ids) = seeded(MODEL).await;
    let committed = commit_record(&rt, MODEL);
    let mut written = Vec::new();
    for i in 0..2 {
        let text = format!("restart must replay this unpublished note {i}");
        let id = write_note(&rt, &token, &text).await;
        bump_generation(&ann, &key).await;
        warm_with_event(&rt, &token, &ann, MODEL).await;
        written.push((id, text));
    }
    assert_eq!(commit_record(&rt, MODEL), committed);
    drop(ann);

    let restarted = new_shared();
    threshold_policy(&restarted);
    let (status, event) = warm_with_event(&rt, &token, &restarted, MODEL).await;
    assert!(
        matches!(status, AnnEnsureStatus::LoadedSnapshot),
        "UNPUBLISHED_RESTART_NO_BUILD: small unpublished tail must replay, got {status:?}"
    );
    assert_eq!(event["path"], "stale_tail_publication");
    assert_eq!(event["ops_applied"], 2);
    assert_eq!(restarted.publication_count.load(Ordering::SeqCst), 1);
    let expected: HashSet<_> = seed_ids
        .into_iter()
        .chain(written.iter().map(|(id, _)| *id))
        .collect();
    {
        let guard = restarted.indexes.read().await;
        let bridge = guard.get(&key).expect("replayed bridge");
        assert_eq!(
            bridge.id_map.iter().copied().collect::<HashSet<_>>(),
            expected
        );
    }
    for (id, text) in written {
        assert_recalled(&rt, &restarted, &key, MODEL, id, &text).await;
    }
}

#[tokio::test]
async fn durable_epoch_change_bypasses_incremental_maintenance() {
    const MODEL: &str = "ann-incremental-epoch-rebuild-model";
    let (rt, token, ann, key, _) = seeded(MODEL).await;
    let committed = commit_record(&rt, MODEL);
    ensure_epoch_schema(&rt)
        .await
        .expect("initialize durable epoch schema");
    let epoch = bump_durable_epoch(&rt)
        .await
        .expect("advance durable epoch");
    maybe_check_durable_epoch(&rt, &ann, &key).await;
    assert!(
        !is_current(&ann, &key).await,
        "epoch change must invalidate the installed bridge"
    );
    let (status, event) = warm_with_event(&rt, &token, &ann, MODEL).await;
    assert!(
        matches!(
            status,
            AnnEnsureStatus::Built {
                vectors: SEED_COUNT
            }
        ),
        "EPOCH_REQUIRES_FULL_BUILD: reindex must not use generation-only maintenance: {status:?}"
    );
    assert_eq!(event["path"], "full_build");
    assert_eq!(event["ops_applied"], 0);
    assert_ne!(commit_record(&rt, MODEL), committed);
    let guard = ann.indexes.read().await;
    assert_eq!(
        guard.get(&key).expect("rebuilt bridge").epoch_baseline,
        epoch
    );
}

#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn completion_events_distinguish_build_from_coalesced_incremental_ops() {
    const MODEL: &str = "ann-incremental-event-model";
    let (rt, token, ann, key, ids) = seeded(MODEL).await;
    let events = completions(&rt, &token).await;
    assert_eq!(events.len(), 1);
    let build = &events[0].payload;
    assert_eq!(
        build["path"], "full_build",
        "FULL_BUILD_EVENT_PATH: initial vector build must be identified"
    );
    assert_eq!(
        build["ops_applied"], 0,
        "a full build applies no write-log delta operations"
    );

    const FINAL_TEXT: &str = "coalesced final embedding for the same memory";
    for text in ["intermediate embedding replaced before warm", FINAL_TEXT] {
        rt.update_note(
            &token,
            ids[0],
            khive_runtime::NotePatch::new(None, Some(text.into()), None, None, None),
        )
        .await
        .expect("update one subject twice");
        bump_generation(&ann, &key).await;
    }
    let (_, event) = warm_with_event(&rt, &token, &ann, MODEL).await;
    assert_eq!(
        event["path"], "incremental_in_place",
        "INCREMENTAL_EVENT_PATH: live delta application must have its own path"
    );
    assert_eq!(
        event["ops_applied"], 1,
        "COALESCED_EVENT_OPS: two updates to one subject apply one final operation"
    );
    for payload in [build, &event] {
        assert_eq!(payload["work_class"], "warm");
        assert_eq!(payload["phase"], "ann_warm");
        assert!(
            payload["wall_us"].is_number(),
            "completion must retain flat wall_us"
        );
        assert!(
            payload.get("cpu_us").is_some(),
            "completion must retain flat cpu_us"
        );
    }
    assert_recalled(&rt, &ann, &key, MODEL, ids[0], FINAL_TEXT).await;
}

#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn checkpoint_consolidates_updates_and_deletes_with_correct_uuid_mapping() {
    const MODEL: &str = "ann-incremental-consolidation-model";
    let (rt, token, ann, key, ids) = seeded(MODEL).await;
    ann.checkpoint_policy
        .write()
        .expect("checkpoint policy")
        .consolidate_tau = 1;
    let publications = ann.publication_count.load(Ordering::SeqCst);
    // Consolidation renumbers ordinals, so it runs only on a full segment
    // rewrite. Leave exactly the four raw operations below before the bound.
    {
        let mut guard = ann.indexes.write().await;
        let bridge = guard.get_mut(&key).expect("seeded bridge");
        bridge.delta_raw_ops = delta::compaction_limit(bridge.base_ops) - 4;
    }
    let updates = [
        (ids[0], "updated first retained memory"),
        (ids[1], "updated second retained memory"),
    ];
    for (id, text) in updates {
        rt.update_note(
            &token,
            id,
            khive_runtime::NotePatch::new(None, Some(text.into()), None, None, None),
        )
        .await
        .expect("update retained note");
    }
    // Two final deletes leave holes even if the updates reused tombstoned slots.
    // A skipped consolidation therefore cannot pass on a fully occupied index.
    for id in [ids[2], ids[3]] {
        assert!(rt
            .delete_note(&token, id, true)
            .await
            .expect("hard delete note"));
    }
    bump_generation(&ann, &key).await;
    let (_, event) = warm_with_event(&rt, &token, &ann, MODEL).await;
    assert_eq!(event["path"], "incremental_checkpoint");
    assert_eq!(event["ops_applied"], 4);
    assert_eq!(
        ann.publication_count.load(Ordering::SeqCst) - publications,
        1
    );
    let expected: HashSet<_> = ids
        .iter()
        .copied()
        .filter(|id| *id != ids[2] && *id != ids[3])
        .collect();
    {
        let guard = ann.indexes.read().await;
        let bridge = guard.get(&key).expect("checkpoint readopted");
        assert_eq!(
            bridge.index.tombstone_count(),
            0,
            "CONSOLIDATION_REMOVES_TOMBSTONES: publication must contain only live slots"
        );
        assert_eq!(bridge.index.num_vectors(), SEED_COUNT - 2);
        assert_eq!(bridge.id_map.len(), bridge.index.num_vectors());
        assert_eq!(bridge.id_map.iter().copied().collect::<HashSet<_>>(), expected, "CONSOLIDATION_UUID_REMAP: compacted ordinals must preserve every live subject exactly once");
        let vectors = bridge.index.vectors().expect("read consolidated vectors");
        for (ordinal, id) in bridge.id_map.iter().enumerate() {
            let original = ids
                .iter()
                .position(|candidate| candidate == id)
                .expect("known live subject");
            let text = if original < 2 {
                updates[original].1.to_string()
            } else {
                format!("incremental seed note {original}")
            };
            let actual = &vectors[ordinal * DIMS..(ordinal + 1) * DIMS];
            assert!(
                exact_cosine(actual, &fnv_to_vec(&text, DIMS)) > 0.99999,
                "CONSOLIDATION_VECTOR_IDENTITY: remapped UUID {id} must still name its own vector"
            );
        }
    }
    for (id, text) in updates {
        assert_recalled(&rt, &ann, &key, MODEL, id, text).await;
    }
    let restarted = new_shared();
    let (status, event) = warm_with_event(&rt, &token, &restarted, MODEL).await;
    assert!(matches!(status, AnnEnsureStatus::LoadedSnapshot));
    assert_eq!(event["path"], "segment_load");
    for (id, text) in updates {
        assert_recalled(&rt, &restarted, &key, MODEL, id, text).await;
    }
}

#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn crossing_delta_limit_rewrites_installed_index_without_corpus_rebuild() {
    const MODEL: &str = "ann-delta-crossing-compaction-model";
    let (rt, token, ann, key, _) = seeded(MODEL).await;
    let dir = ann_segment_dir(&rt, MODEL).expect("segment directory");
    let base_commit = commit_record(&rt, MODEL);
    let publications = ann.publication_count.load(Ordering::SeqCst);
    let mut added = Vec::new();
    {
        let mut indexes = ann.indexes.write().await;
        let bridge = indexes.get_mut(&key).expect("seeded bridge");
        // Leave less cumulative headroom than the next individually cheap
        // tail. The old cap rejected this tail and rebuilt the whole corpus.
        bridge.delta_raw_ops = delta::compaction_limit(bridge.base_ops) - 2;
    }
    for i in 0..3 {
        let text = format!("crossing-limit note {i}");
        added.push((write_note(&rt, &token, &text).await, text));
    }
    bump_generation(&ann, &key).await;
    let (status, event) = warm_with_event(&rt, &token, &ann, MODEL).await;
    assert!(matches!(status, AnnEnsureStatus::AlreadyLoaded));
    assert_eq!(event["path"], "incremental_checkpoint");
    assert_eq!(event["ops_applied"], 3);
    assert_eq!(
        ann.publication_count.load(Ordering::SeqCst) - publications,
        1
    );
    assert_ne!(commit_record(&rt, MODEL), base_commit);
    assert!(
        !dir.join(delta::HEAD_FILE).exists(),
        "crossing the chain limit must rewrite the base checkpoint"
    );
    for (id, text) in added {
        assert_recalled(&rt, &ann, &key, MODEL, id, &text).await;
    }
}

#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn restart_rule_seven_uses_live_fraction_not_delta_headroom() {
    const MODEL: &str = "ann-live-fraction-restart-model";
    let (rt, token, _host, key, ids) = seeded(MODEL).await;
    let dir = ann_segment_dir(&rt, MODEL).expect("segment directory");
    let tail_count = replay_limit(SEED_COUNT as u64, ann_rebuild_threshold()) + 1;
    assert!(tail_count < delta::compaction_limit(SEED_COUNT));
    // Repeated updates leave the live count fixed while the raw replay cost
    // crosses the configured fraction. The delta-chain budget remains ample.
    for i in 0..tail_count {
        rt.update_note(
            &token,
            ids[0],
            khive_runtime::NotePatch::new(
                None,
                Some(format!("restart fraction update {i}")),
                None,
                None,
                None,
            ),
        )
        .await
        .expect("update one seeded note");
    }
    let adopter = new_shared();
    let mut details = AnnWarmDetails::default();
    let outcome =
        classify_and_adopt_segment(&rt, &adopter, &key, MODEL, &dir, 0, 0, &mut details).await;
    assert!(
        matches!(outcome, SegmentOutcome::Cold),
        "rule 8 must choose the rebuild path when raw tail exceeds ceil(f * live)"
    );
    assert_eq!(adopter.publication_count.load(Ordering::SeqCst), 0);
}

#[path = "incremental_edge_tests.rs"]
mod edge_tests;

#[tokio::test]
async fn pathless_incremental_tail_does_not_hold_shared_writer_after_read() {
    const MODEL: &str = "fixture";
    const DIMS: usize = 8;

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    provision_test_vector_store(&rt, MODEL, DIMS);
    let ann = new_shared();
    ann.protected_tail_barrier.store(true, Ordering::SeqCst);

    let reached_pause = ann.protected_tail_notify.notified();
    let task_rt = rt.clone();
    let task_ann = ann.clone();
    let maintenance = tokio::spawn(async move {
        crate::ann::incremental::protected_tail(&task_rt, &task_ann, MODEL, 0, 10)
            .await
            .expect("protected incremental tail")
    });
    tokio::time::timeout(Duration::from_secs(5), reached_pause)
        .await
        .expect("protected tail must reach its pause barrier");

    let writer = rt
        .backend()
        .pool()
        .try_writer_nowait()
        .unwrap_or_else(|error| {
            panic!(
                "PATHLESS_TAIL_WRITER_RELEASE: expected the shared writer to be available while incremental tail maintenance is paused; got {error}"
            )
        });
    drop(writer);

    ann.protected_tail_release.notify_one();
    let tail = maintenance.await.expect("incremental-tail task");
    assert!(
        tail.is_some(),
        "an empty tail remains within the replay limit"
    );
}

fn delta_chunk_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(dir)
        .expect("list segment directory")
        .map(|entry| entry.expect("segment entry").path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("memory_delta-") && name.ends_with(".bin"))
        })
        .collect()
}

#[test]
fn delta_checkpoints_append_only_new_chunks_until_exact_compaction_bound() {
    let temp = tempfile::tempdir().expect("segment root");
    let active_dir = temp.path().join("active-model");
    let idle_dir = temp.path().join("idle-model");
    std::fs::create_dir_all(&active_dir).expect("active model directory");
    std::fs::create_dir_all(&idle_dir).expect("idle model directory");
    let seed = Uuid::new_v4();
    let mut base = AnnBridge::build(vec![1.0, 0.0, 0.0, 0.0], 4, vec![seed], HashSet::new())
        .expect("build base");
    base.set_applied_seq(1);
    base.save_atomic(&active_dir).expect("persist active model");
    base.save_atomic(&idle_dir).expect("persist idle model");
    let base_metadata = std::fs::read(active_dir.join("metadata.bin")).expect("base metadata");
    let idle_files: Vec<_> = [
        "metadata.bin",
        "vectors.bin",
        "graph.bin",
        "lifecycle.bin",
        "codes.bin",
        "external_ids.bin",
    ]
    .into_iter()
    .map(|name| (name, std::fs::read(idle_dir.join(name)).expect("idle file")))
    .collect();

    // A chunk without a committed HEAD is an orphan from a failed publication.
    std::fs::write(
        active_dir.join(format!("memory_delta-{}.bin", Uuid::new_v4())),
        b"orphan",
    )
    .expect("orphan chunk");
    assert!(
        AnnBridge::load(&active_dir).is_ok(),
        "orphan must not affect adoption"
    );

    let mut bridge = AnnBridge::load(&active_dir).expect("load base");
    let mut identities = HashSet::new();
    let mut first_chunk_bytes = None;
    let mut inserted = Vec::new();
    for checkpoint in 1..=4 {
        let id = Uuid::new_v4();
        let vector = vec![0.0, 1.0, checkpoint as f32, 0.0];
        let ops = vec![(id, Some(vector))];
        bridge
            .apply_final_ops(ops.clone(), checkpoint + 1)
            .expect("apply new tail");
        bridge.record_delta_batch(ops, checkpoint + 1, 1_000);
        assert!(
            !bridge.needs_full_compaction(),
            "four checkpoints stay below 5,000 raw ops"
        );
        let before = delta_chunk_files(&active_dir).len();
        let publication = delta::write(&active_dir, &bridge).expect("publish new chunk and HEAD");
        assert_eq!(
            delta_chunk_files(&active_dir).len(),
            before + 1,
            "each checkpoint writes exactly one new immutable chunk"
        );
        let chunk_bytes = std::fs::metadata(
            active_dir.join(format!("memory_delta-{}.bin", publication.last_nonce)),
        )
        .expect("new chunk")
        .len();
        assert_eq!(
            chunk_bytes,
            *first_chunk_bytes.get_or_insert(chunk_bytes),
            "later checkpoints must write only their fresh one-op tail"
        );
        assert!(
            identities.insert(publication.identity),
            "each publication gets a fresh identity"
        );
        assert_eq!(
            std::fs::read(active_dir.join("metadata.bin")).unwrap(),
            base_metadata,
            "delta publication must preserve the base segment nonce"
        );
        bridge.commit_digest = Some(publication.identity);
        bridge.last_delta_nonce = Some(publication.last_nonce);
        bridge.delta_batches.clear();
        bridge.mark_checkpointed();
        bridge = AnnBridge::load(&active_dir).expect("adopter replays committed chain");
        assert_eq!(bridge.commit_digest, Some(publication.identity));
        assert!(
            bridge.id_map.contains(&id),
            "adopter must see the delta's new vector"
        );
        inserted.push(id);
    }
    for (name, bytes) in &idle_files {
        assert_eq!(
            std::fs::read(idle_dir.join(name)).unwrap(),
            *bytes,
            "idle second MODEL must remain byte-identical: {name}"
        );
    }

    let final_id = Uuid::new_v4();
    let ops = vec![(final_id, Some(vec![0.0, 0.0, 0.0, 1.0]))];
    bridge
        .apply_final_ops(ops.clone(), 6)
        .expect("apply threshold tail");
    bridge.record_delta_batch(ops, 6, 1_000);
    assert!(
        bridge.needs_full_compaction(),
        "exactly 5,000 raw ops require one full rewrite"
    );
    bridge.save_atomic(&active_dir).expect("full compaction");
    assert_ne!(
        std::fs::read(active_dir.join("metadata.bin")).unwrap(),
        base_metadata
    );
    assert!(
        !active_dir.join(delta::HEAD_FILE).exists(),
        "compaction empties delta HEAD"
    );
    assert!(
        delta_chunk_files(&active_dir).is_empty(),
        "compaction reclaims old chunks and orphans"
    );
    let adopted = AnnBridge::load(&active_dir).expect("adopt compacted segment");
    assert!(adopted.id_map.contains(&final_id));
    assert!(inserted.iter().all(|id| adopted.id_map.contains(id)));
}

#[test]
fn compaction_metadata_before_head_cleanup_adopts_new_base() {
    let temp = tempfile::tempdir().expect("segment directory");
    let dir = temp.path();
    let seed = Uuid::new_v4();
    let added = Uuid::new_v4();
    let mut base = AnnBridge::build(vec![1.0, 0.0, 0.0, 0.0], 4, vec![seed], HashSet::new())
        .expect("build base");
    base.set_applied_seq(1);
    base.save_atomic(dir).expect("persist base");
    let mut bridge = AnnBridge::load(dir).expect("load base");
    let ops = vec![(added, Some(vec![0.0, 1.0, 0.0, 0.0]))];
    bridge.apply_final_ops(ops.clone(), 2).expect("apply tail");
    bridge.record_delta_batch(ops, 2, 1);
    delta::write(dir, &bridge).expect("publish delta");
    assert!(dir.join(delta::HEAD_FILE).exists());

    // Simulate a crash after v2 metadata + external IDs commit but before
    // removing the old HEAD. Its watermark is covered by the new base.
    bridge
        .index
        .save_atomic(dir)
        .expect("commit new base files");
    let digest = segment_commit_digest(dir)
        .expect("read commit")
        .expect("commit exists");
    write_external_ids_sidecar(dir, &digest, &bridge.id_map).expect("commit external IDs");
    assert!(
        dir.join(delta::HEAD_FILE).exists(),
        "old HEAD remains during crash window"
    );
    let adopted = AnnBridge::load(dir).expect("stale HEAD must not reject new base");
    assert!(adopted.id_map.contains(&added));
    assert_eq!(delta::publication_digest(dir).unwrap(), Some(digest));
    delta::clear(dir).expect("clear old chain");
    assert!(!dir.join(delta::HEAD_FILE).exists());
}

#[test]
fn delta_publication_bytes_follow_new_ops_across_hundredfold_corpus() {
    fn one_checkpoint(count: usize) -> (u64, u64) {
        let temp = tempfile::tempdir().expect("segment directory");
        let dir = temp.path();
        let mut vectors = Vec::with_capacity(count * 4);
        let mut ids = Vec::with_capacity(count);
        for i in 0..count {
            vectors.extend_from_slice(&[1.0, (i % 17) as f32, (i / 17) as f32, 0.5]);
            ids.push(Uuid::new_v4());
        }
        let mut base = AnnBridge::build(vectors, 4, ids, HashSet::new()).expect("build corpus");
        base.set_applied_seq(1);
        base.save_atomic(dir).expect("persist corpus");
        let base_bytes: u64 = [
            "metadata.bin",
            "vectors.bin",
            "graph.bin",
            "lifecycle.bin",
            "codes.bin",
            "external_ids.bin",
        ]
        .into_iter()
        .map(|name| std::fs::metadata(dir.join(name)).unwrap().len())
        .sum();
        let mut bridge = AnnBridge::load(dir).expect("load corpus");
        let id = Uuid::new_v4();
        let ops = vec![(id, Some(vec![0.0, 0.0, 1.0, 0.0]))];
        bridge
            .apply_final_ops(ops.clone(), 2)
            .expect("apply one new operation");
        bridge.record_delta_batch(ops, 2, 1);
        delta::write(dir, &bridge).expect("publish delta");
        let delta_bytes = std::fs::metadata(dir.join(delta::HEAD_FILE)).unwrap().len()
            + delta_chunk_files(dir)
                .into_iter()
                .map(|path| std::fs::metadata(path).unwrap().len())
                .sum::<u64>();
        (base_bytes, delta_bytes)
    }
    let (small_base, small_delta) = one_checkpoint(2);
    let (large_base, large_delta) = one_checkpoint(200);
    assert!(
        large_base > small_base * 10,
        "the fixture must materially enlarge the base segment"
    );
    assert_eq!(
        large_delta, small_delta,
        "a 100x corpus with one new op must write the same delta bytes"
    );
}

#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn non_owner_adopts_published_delta_without_build_or_publication() {
    const MODEL: &str = "ann-delta-non-owner-adoption-model";
    let (rt, token, host, key, _) = seeded(MODEL).await;
    let mut final_note = None;
    for i in 0..4 {
        let text = format!("published delta note {i}");
        let id = write_note(&rt, &token, &text).await;
        bump_generation(&host, &key).await;
        warm_with_event(&rt, &token, &host, MODEL).await;
        final_note = Some((id, text));
    }
    let dir = ann_segment_dir(&rt, MODEL).expect("segment directory");
    let head = std::fs::read(dir.join(delta::HEAD_FILE)).expect("published delta HEAD");
    let metadata = commit_record(&rt, MODEL);
    let chunks = delta_chunk_files(&dir).len();

    let client = new_shared_for_role(false);
    let (status, event) = warm_with_event(&rt, &token, &client, MODEL).await;
    assert!(matches!(status, AnnEnsureStatus::LoadedSnapshot));
    assert_eq!(event["path"], "segment_load");
    assert_eq!(client.segment_load_count.load(Ordering::SeqCst), 1);
    assert_eq!(client.publication_count.load(Ordering::SeqCst), 0);
    let (id, text) = final_note.expect("fourth note");
    assert_recalled(&rt, &client, &key, MODEL, id, &text).await;
    assert_eq!(std::fs::read(dir.join(delta::HEAD_FILE)).unwrap(), head);
    assert_eq!(commit_record(&rt, MODEL), metadata);
    assert_eq!(delta_chunk_files(&dir).len(), chunks);
}

#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn stale_tail_publication_recounts_write_between_branch_and_replay() {
    const MODEL: &str = "ann-stale-tail-branch-race-model";
    let (rt, token, _seed_host, key, _) = seeded(MODEL).await;
    let dir = ann_segment_dir(&rt, MODEL).expect("segment directory");
    let base_seq = read_commit_info(&dir)
        .expect("read base commit")
        .expect("base commit")
        .last_applied_seq
        .expect("base watermark");
    let base_digest = segment_commit_digest(&dir)
        .expect("read base identity")
        .expect("base identity");
    let first = write_note(&rt, &token, "first branch-tail write").await;

    let adopter = new_shared();
    adopter
        .stale_tail_scope_barrier
        .store(true, Ordering::SeqCst);
    let paused = adopter.stale_tail_scope_notify.notified();
    let task_rt = rt.clone();
    let task_ann = adopter.clone();
    let task_dir = dir.clone();
    let task = tokio::spawn(async move {
        let mut details = AnnWarmDetails::default();
        classify_and_adopt_segment(
            &task_rt,
            &task_ann,
            &key,
            MODEL,
            &task_dir,
            0,
            0,
            &mut details,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), paused)
        .await
        .expect("classifier must pause after branch count");
    let second = write_note(&rt, &token, "write between count and replay").await;
    adopter.stale_tail_scope_release.notify_one();

    assert!(matches!(
        task.await.expect("adopter task"),
        SegmentOutcome::Installed(AnnEnsureStatus::LoadedSnapshot)
    ));
    let (_, raw_count) = delta::read_info(&dir, &base_digest, base_seq)
        .expect("read delta HEAD")
        .expect("HEAD exists");
    assert_eq!(raw_count, 2, "the replay snapshot must count both writes");
    let published = AnnBridge::load(&dir).expect("replay published delta");
    assert!(published.id_map.contains(&first));
    assert!(published.id_map.contains(&second));
}

#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn stale_tail_publication_counts_only_ops_in_its_protected_snapshot() {
    const MODEL: &str = "ann-stale-tail-one-snapshot-model";
    let (rt, token, _seed_host, key, _) = seeded(MODEL).await;
    let dir = ann_segment_dir(&rt, MODEL).expect("segment directory");
    let base_info = read_commit_info(&dir)
        .expect("read base commit")
        .expect("base commit exists");
    let base_seq = base_info.last_applied_seq.expect("base watermark");
    let base_digest = segment_commit_digest(&dir)
        .expect("read base identity")
        .expect("base identity exists");
    let first = write_note(&rt, &token, "first stale-tail write").await;

    let adopter = new_shared();
    adopter.protected_tail_barrier.store(true, Ordering::SeqCst);
    let paused = adopter.protected_tail_notify.notified();
    let task_rt = rt.clone();
    let task_ann = adopter.clone();
    let task_key = key.clone();
    let task_dir = dir.clone();
    let task = tokio::spawn(async move {
        let mut details = AnnWarmDetails::default();
        classify_and_adopt_segment(
            &task_rt,
            &task_ann,
            &task_key,
            MODEL,
            &task_dir,
            0,
            0,
            &mut details,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), paused)
        .await
        .expect("stale-tail replay must reach the protected snapshot seam");
    let later = write_note(&rt, &token, "write after the protected snapshot").await;
    adopter.protected_tail_release.notify_one();

    assert!(matches!(
        task.await.expect("adopter task"),
        SegmentOutcome::Installed(AnnEnsureStatus::LoadedSnapshot)
    ));
    let (published_seq, raw_count) = delta::read_info(&dir, &base_digest, base_seq)
        .expect("read delta HEAD")
        .expect("HEAD exists");
    assert_eq!(
        raw_count, 1,
        "the HEAD counts the selected raw row, not the later write"
    );
    let published = AnnBridge::load(&dir).expect("replay published delta");
    assert!(published.id_map.contains(&first));
    assert!(!published.id_map.contains(&later));
    assert!(tail_exists(&rt, MODEL, published_seq)
        .await
        .expect("later write remains in the SQL tail"));
}

#[cfg(unix)]
#[test]
fn memory_delta_reader_rejects_linked_head() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("segment root");
    let dir = temp.path().join("model");
    std::fs::create_dir_all(&dir).expect("segment directory");
    let mut base = AnnBridge::build(
        vec![1.0, 0.0, 0.0, 0.0],
        4,
        vec![Uuid::new_v4()],
        HashSet::new(),
    )
    .expect("build base");
    base.set_applied_seq(1);
    base.save_atomic(&dir).expect("persist base");
    let outside = temp.path().join("outside");
    std::fs::write(&outside, [0u8; 112]).expect("outside file");
    symlink(&outside, dir.join(delta::HEAD_FILE)).expect("plant linked HEAD");
    let reader = khive_vamana::AuxiliarySidecarReader::open(&dir).expect("pinned reader");
    assert!(
        reader.read_bounded(delta::HEAD_FILE, 112).is_err(),
        "the pinned reader must reject the final symlink itself"
    );
    assert!(
        AnnBridge::load(&dir).is_err(),
        "a linked delta HEAD must be rejected before following it"
    );
}

#[test]
fn memory_delta_reader_rejects_oversized_head_and_chunk() {
    let temp = tempfile::tempdir().expect("segment root");
    let dir = temp.path();
    let mut base = AnnBridge::build(
        vec![1.0, 0.0, 0.0, 0.0],
        4,
        vec![Uuid::new_v4()],
        HashSet::new(),
    )
    .expect("build base");
    base.set_applied_seq(1);
    base.save_atomic(dir).expect("persist base");
    std::fs::write(dir.join(delta::HEAD_FILE), [0u8; 113]).expect("oversized HEAD");
    let reader = khive_vamana::AuxiliarySidecarReader::open(dir).expect("pinned reader");
    assert!(reader.read_bounded(delta::HEAD_FILE, 112).is_err());
    assert!(
        AnnBridge::load(dir).is_err(),
        "HEAD must be size-capped before allocation"
    );
    std::fs::remove_file(dir.join(delta::HEAD_FILE)).expect("remove malformed HEAD");

    let mut bridge = AnnBridge::load(dir).expect("reload base");
    let ops = vec![(Uuid::new_v4(), Some(vec![0.0, 1.0, 0.0, 0.0]))];
    bridge.apply_final_ops(ops.clone(), 2).expect("apply delta");
    bridge.record_delta_batch(ops, 2, 1);
    let published = delta::write(dir, &bridge).expect("publish delta");
    let chunk = dir.join(format!("memory_delta-{}.bin", published.last_nonce));
    let valid_len = std::fs::metadata(&chunk).expect("chunk size").len();
    std::fs::write(&chunk, vec![0u8; valid_len as usize + 1]).expect("oversized chunk");
    assert!(reader
        .read_bounded(
            &format!("memory_delta-{}.bin", published.last_nonce),
            valid_len as usize,
        )
        .is_err());
    assert!(
        AnnBridge::load(dir).is_err(),
        "chunk must be size-capped before allocation"
    );
}

#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn service_checkpoint_below_bound_appends_delta_and_keeps_base_files() {
    const MODEL: &str = "ann-service-delta-checkpoint-model";
    const BASE_FILES: [&str; 6] = [
        "metadata.bin",
        "vectors.bin",
        "graph.bin",
        "lifecycle.bin",
        "codes.bin",
        "external_ids.bin",
    ];
    let (rt, token, ann, key, _) = seeded(MODEL).await;
    let dir = ann_segment_dir(&rt, MODEL).expect("segment directory");
    let base: Vec<_> = BASE_FILES
        .into_iter()
        .map(|name| (name, std::fs::read(dir.join(name)).expect("base file")))
        .collect();
    assert!(
        !dir.join(delta::HEAD_FILE).exists() && delta_chunk_files(&dir).is_empty(),
        "a fresh build publishes no delta"
    );
    let publications = ann.publication_count.load(Ordering::SeqCst);

    let mut last = None;
    for i in 0..4 {
        let text = format!("service delta checkpoint note {i}");
        let id = write_note(&rt, &token, &text).await;
        bump_generation(&ann, &key).await;
        warm_with_event(&rt, &token, &ann, MODEL).await;
        last = Some((id, text));
    }

    assert_eq!(
        ann.publication_count.load(Ordering::SeqCst) - publications,
        1,
        "the fourth dirty operation publishes exactly once"
    );
    for (name, bytes) in &base {
        assert_eq!(
            std::fs::read(dir.join(name)).expect("base file after checkpoint"),
            *bytes,
            "a checkpoint below the compaction bound must not rewrite {name}"
        );
    }
    assert!(
        dir.join(delta::HEAD_FILE).exists(),
        "the checkpoint must publish a delta HEAD"
    );
    assert_eq!(
        delta_chunk_files(&dir).len(),
        4,
        "the checkpoint must append one delta chunk per warmed batch"
    );

    let restarted = new_shared_for_role(false);
    let (status, _) = warm_with_event(&rt, &token, &restarted, MODEL).await;
    assert!(matches!(status, AnnEnsureStatus::LoadedSnapshot));
    let (id, text) = last.expect("last written note");
    assert_recalled(&rt, &restarted, &key, MODEL, id, &text).await;
}

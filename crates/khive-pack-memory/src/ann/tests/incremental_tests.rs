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
    assert_ne!(
        commit_record(&rt, MODEL),
        committed,
        "threshold must rotate the commit record"
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

#[path = "incremental_edge_tests.rs"]
mod edge_tests;

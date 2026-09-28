use super::*;

#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn repeated_subject_updates_count_raw_rows_toward_checkpoint() {
    const MODEL: &str = "ann-incremental-raw-row-threshold-model";
    let (rt, token, ann, key, ids) = seeded(MODEL).await;
    let committed = commit_record(&rt, MODEL);
    let publications = ann.publication_count.load(Ordering::SeqCst);
    let loads = ann.segment_load_count.load(Ordering::SeqCst);

    for cycle in 0..2 {
        let final_text = format!("raw-row threshold final value {cycle}");
        for text in [
            format!("raw-row threshold intermediate value {cycle}"),
            final_text.clone(),
        ] {
            rt.update_note(
                &token,
                ids[0],
                khive_runtime::NotePatch::new(None, Some(text), None, None, None),
            )
            .await
            .expect("update the same memory twice before warming");
            bump_generation(&ann, &key).await;
        }
        let (_, event) = warm_with_event(&rt, &token, &ann, MODEL).await;
        assert_eq!(
            event["ops_applied"], 1,
            "RAW_DIRTY_LOG_COUNT: each pair coalesces to one applied operation"
        );
        assert_eq!(
            ann.publication_count.load(Ordering::SeqCst) - publications,
            cycle,
            "RAW_DIRTY_LOG_COUNT: the fourth raw row must publish despite coalescing"
        );
        assert_eq!(
            event["path"],
            if cycle == 0 {
                "incremental_in_place"
            } else {
                "incremental_checkpoint"
            },
            "RAW_DIRTY_LOG_COUNT: checkpoint cadence must follow raw rows"
        );
        if cycle == 0 {
            assert_eq!(commit_record(&rt, MODEL), committed);
            assert_eq!(ann.segment_load_count.load(Ordering::SeqCst), loads);
            let indexes = ann.indexes.read().await;
            assert_eq!(
                indexes.get(&key).unwrap().dirty_ops,
                2,
                "RAW_DIRTY_LOG_COUNT: both coalesced rows must remain dirty"
            );
        } else {
            assert_ne!(commit_record(&rt, MODEL), committed);
            assert_eq!(ann.indexes.read().await.get(&key).unwrap().dirty_ops, 0);
        }
        assert_recalled(&rt, &ann, &key, MODEL, ids[0], &final_text).await;
    }
}

#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn elapsed_dirty_interval_checkpoints_without_generation_change() {
    const MODEL: &str = "ann-incremental-elapsed-interval-model";
    let (rt, token, ann, key, _) = seeded(MODEL).await;
    let text = "dirty interval checkpoint retains this new memory";
    let id = write_note(&rt, &token, text).await;
    bump_generation(&ann, &key).await;
    warm_with_event(&rt, &token, &ann, MODEL).await;
    assert!(is_current(&ann, &key).await);
    let generation = current_generation(&ann, &key).await;
    let publications = ann.publication_count.load(Ordering::SeqCst);
    let committed = commit_record(&rt, MODEL);
    let before: HashSet<_> = completions(&rt, &token)
        .await
        .into_iter()
        .map(|event| event.id)
        .collect();
    let interval = Duration::from_secs(1);
    ann.checkpoint_policy
        .write()
        .expect("checkpoint policy")
        .interval = interval;
    {
        let mut indexes = ann.indexes.write().await;
        let bridge = indexes.get_mut(&key).expect("dirty bridge");
        assert_eq!(bridge.dirty_ops, 1);
        bridge.last_checkpoint = std::time::Instant::now() - interval;
    }

    let status = ensure_ann_for_model(&rt, &token, &ann, MODEL)
        .await
        .expect("elapsed interval warm");
    assert_eq!(
        ann.publication_count.load(Ordering::SeqCst) - publications,
        1,
        "INTERVAL_DIRTY_CHECKPOINT: elapsed dirty state must publish without another generation bump; {status:?}"
    );
    assert_ne!(
        commit_record(&rt, MODEL),
        committed,
        "INTERVAL_DIRTY_CHECKPOINT: interval must rotate the segment commit"
    );
    assert_eq!(
        current_generation(&ann, &key).await,
        generation,
        "interval fixture must not introduce a write-generation change"
    );
    assert!(is_current(&ann, &key).await);
    assert_eq!(ann.indexes.read().await.get(&key).unwrap().dirty_ops, 0);
    let added: Vec<_> = completions(&rt, &token)
        .await
        .into_iter()
        .filter(|event| !before.contains(&event.id))
        .collect();
    assert_eq!(
        added.len(),
        1,
        "INTERVAL_DIRTY_CHECKPOINT: elapsed maintenance must emit a completion"
    );
    assert_eq!(added[0].payload["path"], "incremental_checkpoint");
    assert_eq!(added[0].payload["ops_applied"], 0);
    assert_recalled(&rt, &ann, &key, MODEL, id, text).await;
}

#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn dirty_delete_rotation_adopts_peer_floor_and_recovers_unpublished_tail() {
    const MODEL: &str = "ann-incremental-dirty-rotation-model";
    let (rt, token, ann, key, seed_ids) = seeded(MODEL).await;
    let dir = ann_segment_dir(&rt, MODEL).expect("segment directory");
    let published = bridge_applied_seq(&ann, &key)
        .await
        .expect("published seed watermark");
    let publications = ann.publication_count.load(Ordering::SeqCst);
    let probe = Arc::new(());
    let predecessor_dropped = Arc::downgrade(&probe);
    ann.indexes.write().await.get_mut(&key).unwrap().drop_probe = Some(probe);

    // Deletion changes only the graph/lifecycle, retaining the loaded vector
    // mapping until the peer rotation replaces this dirty bridge.
    assert!(rt
        .delete_note(&token, seed_ids[0], true)
        .await
        .expect("delete seeded note"));
    bump_generation(&ann, &key).await;
    let (_, event) = warm_with_event(&rt, &token, &ann, MODEL).await;
    assert_eq!(event["path"], "incremental_in_place");
    assert_eq!(event["ops_applied"], 1);
    assert!(
        is_current(&ann, &key).await,
        "deletion replay must be current before peer rotation"
    );
    let (applied, incumbent_digest) = {
        let indexes = ann.indexes.read().await;
        let bridge = indexes.get(&key).unwrap();
        assert_eq!(bridge.dirty_ops, 1);
        assert_eq!(bridge.index.tombstone_count(), 1);
        assert_eq!(bridge.published_seq, published);
        (
            bridge.index.last_applied_seq().unwrap(),
            bridge.commit_digest.unwrap(),
        )
    };
    assert!(applied > published);
    let generation = current_generation(&ann, &key).await;
    let peer =
        AnnBridge::load(&dir).expect("load the unchanged committed segment for peer publication");
    assert_eq!(peer.index.last_applied_seq(), Some(published));
    peer.save_atomic(&dir)
        .expect("peer rotates a valid checkpoint at the published floor");
    drop(peer);
    let peer_digest = segment_commit_digest(&dir).unwrap().unwrap();
    assert_ne!(peer_digest, incumbent_digest);

    refresh_rotated_segments_once(&rt, &ann).await;
    assert!(
        predecessor_dropped.upgrade().is_none(),
        "DIRTY_ROTATION_RETAINS_TAIL: rotation must release the dirty predecessor mmap owner"
    );
    {
        let indexes = ann.indexes.read().await;
        let replacement = indexes.get(&key).expect("DIRTY_ROTATION_RETAINS_TAIL: a valid peer checkpoint at the published floor must stay installed");
        assert_eq!(replacement.commit_digest, Some(peer_digest));
        assert_eq!(replacement.index.last_applied_seq(), Some(published));
        assert_eq!(replacement.published_seq, published);
    }
    assert_eq!(
        current_generation(&ann, &key).await,
        generation + 1,
        "DIRTY_ROTATION_RETAINS_TAIL: adopting a peer behind applied progress must request replay"
    );
    assert!(
        !is_current(&ann, &key).await,
        "DIRTY_ROTATION_RETAINS_TAIL: peer adoption must retain freshness pressure"
    );
    assert_eq!(ann.publication_count.load(Ordering::SeqCst), publications);

    let deleted_query = fnv_to_vec("incremental seed note 0", DIMS);
    let (raw, seq) = search_loaded_with_seq(&ann, &key, &deleted_query, SEED_COUNT + 8)
        .await
        .expect("search adopted peer")
        .expect("peer bridge installed");
    assert_eq!(seq, published);
    assert!(
        raw.iter().any(|(id, _)| *id == seed_ids[0]),
        "peer checkpoint must still contain the deleted subject before exact-tail repair"
    );
    let outcome = fresh_tail_leg(
        &rt,
        &ann,
        &key,
        MODEL,
        &deleted_query,
        SEED_COUNT + 8,
        Some(seq),
    )
    .await;
    let (merged, reason) = outcome_into_candidates(outcome, raw, &deleted_query);
    assert_eq!(
        reason, None,
        "DIRTY_ROTATION_RETAINS_TAIL: retained deletion tail must remain readable"
    );
    assert!(
        !merged.iter().any(|(id, _)| *id == seed_ids[0]),
        "DIRTY_ROTATION_RETAINS_TAIL: exact tail must remove the peer's stale deleted subject"
    );

    let mut written = Vec::new();
    for text in [
        "first new memory after dirty peer rotation",
        "second new memory after dirty peer rotation",
    ] {
        let id = write_note(&rt, &token, text).await;
        bump_generation(&ann, &key).await;
        assert_recalled(&rt, &ann, &key, MODEL, id, text).await;
        written.push((id, text));
    }
    let (_, recovered) = warm_with_event(&rt, &token, &ann, MODEL).await;
    assert_eq!(recovered["path"], "incremental_in_place");
    assert_eq!(recovered["ops_applied"], 3);
    assert!(
        is_current(&ann, &key).await,
        "DIRTY_ROTATION_RETAINS_TAIL: later warm must recover all unpublished operations"
    );
    let expected: HashSet<_> = seed_ids
        .iter()
        .copied()
        .skip(1)
        .chain(written.iter().map(|(id, _)| *id))
        .collect();
    {
        let indexes = ann.indexes.read().await;
        let bridge = indexes.get(&key).unwrap();
        let live_ids: HashSet<_> = bridge
            .id_map
            .iter()
            .enumerate()
            .filter(|(ordinal, _)| !bridge.index.is_tombstoned(*ordinal as u32))
            .map(|(_, id)| *id)
            .collect();
        assert_eq!(live_ids, expected, "DIRTY_ROTATION_RETAINS_TAIL: replay must recover every new write and preserve the deletion");
    }
    for (id, text) in written {
        assert_recalled(&rt, &ann, &key, MODEL, id, text).await;
    }
}

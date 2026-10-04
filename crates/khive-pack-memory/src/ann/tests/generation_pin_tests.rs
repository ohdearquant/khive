use super::*;

/// Assert the leg replaced its candidates with nothing and disclosed `expected_reason`.
fn assert_dropped_with_reason(outcome: FreshTailOutcome, expected_reason: &'static str) {
    match outcome {
        FreshTailOutcome::Replace(candidates, reason) => {
            assert!(
                candidates.is_empty(),
                "the stale candidates must be dropped, got: {candidates:?}"
            );
            assert_eq!(reason, Some(expected_reason));
        }
        FreshTailOutcome::Ops(ops) => {
            panic!("expected a dropped replacement, got ops: {ops:?}")
        }
        FreshTailOutcome::Skipped(reason) => {
            panic!("expected a dropped replacement, got a skip: {reason}")
        }
    }
}

/// A build stamped with `captured_generation` before a bump still installs after it. The
/// install rule compares the candidate with the installed entry only, so the bump leaves the
/// installed build behind the counter and the next request schedules a rebuild.
async fn assert_late_build_installs_below_the_counter(
    ann: &SharedAnn,
    key: &AnnKey,
    captured_generation: u64,
) {
    let late_id = Uuid::new_v4();
    let late = tiny_bridge(late_id, captured_generation);
    assert!(
        install_replacing(ann, key, late).await,
        "a build captured before the bump must still install"
    );
    {
        let installed = ann.indexes.read().await;
        let bridge = installed.get(key).expect("the late build is installed");
        assert_eq!(bridge.id_map, vec![late_id]);
        assert_eq!(bridge.generation, captured_generation);
    }
    assert!(
        current_generation(ann, key).await > captured_generation,
        "the bump must have moved the counter past the build's generation"
    );
    assert!(
        !is_current(ann, key).await,
        "the installed build predates the bump, so it must not read as current"
    );
}

/// A pathless mismatch with no installed bridge drops the stale candidates and bumps the
/// write generation; a build captured before that bump still installs afterwards.
#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn fresh_tail_pathless_reresolve_without_a_bridge_bumps_the_generation() {
    const MODEL: &str = "adr118-pathless-no-bridge-bump-model";
    const DIMS: usize = 8;
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let ann = new_shared();
    let key = AnnKey::from_token(MODEL);
    let query = fnv_to_vec("pathless no bridge bump", DIMS);
    let captured_generation = current_generation(&ann, &key).await;

    let outcome = fresh_tail_pathless_reresolve(
        &rt,
        &ann,
        &key,
        MODEL,
        FreshTailSearch::new(&query, 10, AnnScoreRoute::Memory),
        None,
    )
    .await;

    assert_dropped_with_reason(
        outcome,
        "fresh-tail: pathless mismatch has no installed bridge; dropped stale candidates",
    );
    assert_eq!(
        current_generation(&ann, &key).await,
        captured_generation + 1,
        "the no-bridge branch must bump the write generation exactly once"
    );
    assert_late_build_installs_below_the_counter(&ann, &key, captured_generation).await;
}

/// A pathless mismatch whose search on the installed bridge fails drops the stale candidates
/// and bumps the write generation without emptying the slot; a build captured before that
/// bump still installs afterwards.
#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn fresh_tail_pathless_reresolve_search_failure_bumps_the_generation() {
    const MODEL: &str = "adr118-pathless-search-failure-bump-model";
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let ann = new_shared();
    let key = AnnKey::from_token(MODEL);
    let seeded = install_replacing(&ann, &key, tiny_bridge(Uuid::new_v4(), 0)).await;
    assert!(seeded, "the incumbent must install into the empty slot");
    // The installed bridge is 4 wide, so a 5 wide query makes its search fail.
    let query = vec![0.5_f32; 5];
    let search_fails = {
        let installed = ann.indexes.read().await;
        let bridge = installed.get(&key).expect("incumbent installed");
        bridge
            .search_with_route(&query, 10, AnnScoreRoute::Memory)
            .is_err()
    };
    assert!(
        search_fails,
        "precondition: the installed bridge must fail this query"
    );
    let captured_generation = current_generation(&ann, &key).await;

    let outcome = fresh_tail_pathless_reresolve(
        &rt,
        &ann,
        &key,
        MODEL,
        FreshTailSearch::new(&query, 10, AnnScoreRoute::Memory),
        None,
    )
    .await;

    assert_dropped_with_reason(
        outcome,
        "fresh-tail: pathless re-resolution search failed; dropped stale candidates",
    );
    assert_eq!(
        current_generation(&ann, &key).await,
        captured_generation + 1,
        "the failed-search branch must bump the write generation exactly once"
    );
    assert!(
        ann.indexes.read().await.contains_key(&key),
        "a bump never empties the installed slot"
    );
    assert_late_build_installs_below_the_counter(&ann, &key, captured_generation).await;
}

/// A pathless mismatch whose installed bridge sits below the registry floor drops the stale
/// candidates and bumps the write generation; a build captured before that bump still
/// installs afterwards.
#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn fresh_tail_pathless_reresolve_below_the_registry_floor_bumps_the_generation() {
    const MODEL: &str = "adr118-pathless-floor-bump-model";
    const DIMS: usize = 8;
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    rt.register_embedder(HashVecProvider {
        model_name: MODEL.to_owned(),
        dims: DIMS,
    });
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    for i in 0..3u32 {
        rt.create_note_with_decay_for_embedding_model(
            &token,
            "memory",
            None,
            &format!("pathless floor baseline note {i}"),
            Some(0.7),
            0.01,
            None,
            vec![],
            None,
        )
        .await
        .expect("create baseline note");
    }

    let ann = new_shared();
    let key = AnnKey::from_token(MODEL);
    ensure_ann_for_model(&rt, &token, &ann, MODEL)
        .await
        .expect("warm pathless bridge");
    let resolved_s = bridge_applied_seq(&ann, &key)
        .await
        .expect("installed bridge watermark");
    // A peer checkpoint raises the shared registry floor past the installed bridge.
    raise_watermark(&rt, MODEL, resolved_s + 1)
        .await
        .expect("raise registry floor above the bridge");
    let captured_generation = current_generation(&ann, &key).await;
    let query = fnv_to_vec("pathless floor baseline note 0", DIMS);

    let outcome = fresh_tail_pathless_reresolve(
        &rt,
        &ann,
        &key,
        MODEL,
        FreshTailSearch::new(&query, 10, AnnScoreRoute::Memory),
        None,
    )
    .await;

    assert_dropped_with_reason(
        outcome,
        "fresh-tail: pathless mismatch has no bridge at the registry floor; dropped stale candidates",
    );
    assert_eq!(
        current_generation(&ann, &key).await,
        captured_generation + 1,
        "the below-floor branch must bump the write generation exactly once"
    );
    assert_late_build_installs_below_the_counter(&ann, &key, captured_generation).await;
}

/// Re-resolution with no segment directory drops the stale candidates and bumps the write
/// generation without emptying the slot; a build captured before that bump still installs
/// afterwards. Serving reaches this branch only if the directory disappears between the
/// mismatch check and the reload, so the test calls it directly on an in-memory backend.
#[tokio::test]
#[serial(adr118_fresh_tail)]
async fn fresh_tail_reresolve_without_a_segment_directory_bumps_the_generation() {
    const MODEL: &str = "adr118-reresolve-no-directory-bump-model";
    const DIMS: usize = 8;
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    assert!(
        ann_segment_dir(&rt, MODEL).is_none(),
        "precondition: an in-memory backend has no segment directory"
    );
    let ann = new_shared();
    let key = AnnKey::from_token(MODEL);
    let seeded = install_replacing(&ann, &key, tiny_bridge(Uuid::new_v4(), 0)).await;
    assert!(seeded, "the incumbent must install into the empty slot");
    let captured_generation = current_generation(&ann, &key).await;
    let query = fnv_to_vec("reresolve no directory bump", DIMS);

    let outcome = fresh_tail_reresolve(
        &rt,
        &ann,
        &key,
        MODEL,
        FreshTailSearch::new(&query, 10, AnnScoreRoute::Memory),
        1,
        None,
    )
    .await;

    assert_dropped_with_reason(
        outcome,
        "fresh-tail: re-resolved segment directory unavailable; dropped stale candidates",
    );
    assert_eq!(
        current_generation(&ann, &key).await,
        captured_generation + 1,
        "the no-directory branch must bump the write generation exactly once"
    );
    assert!(
        ann.indexes.read().await.contains_key(&key),
        "a bump never empties the installed slot"
    );
    assert_late_build_installs_below_the_counter(&ann, &key, captured_generation).await;
}

/// `memory.remember` bumps the write generation of every registered model when it writes a
/// new row; a build captured before that bump still installs afterwards.
///
/// The request names no `embedding_model`: that field resolves built-in model names only, so a
/// test embedder is reached through the registered-model default, as the neighbouring handler
/// tests do. The runtime registers exactly one model, so the write bumps exactly one counter.
/// No note-mutation hook is installed on this pack, so the bump observed here is the
/// handler's own. The warm it queues is parked at its first attempt, so it cannot install
/// anything while the test reads the counter and installs the late build.
#[tokio::test]
#[serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn remember_bumps_the_generation_and_a_prior_build_still_installs() {
    const MODEL: &str = "adr079-remember-bump-pin-model";
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    rt.register_embedder(HashVecProvider {
        model_name: MODEL.to_owned(),
        dims: 8,
    });
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    let pack = crate::MemoryPack::new(rt.clone());
    let ann = pack.ann_for_test();
    let key = AnnKey::from_token(MODEL);
    let captured_generation = current_generation(&ann, &key).await;

    ann.attempt_floor_barrier
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let warm_parked = ann.attempt_floor_notify.notified();
    pack.handle_remember(
        &token,
        serde_json::json!({
            "content": "remember generation pin note",
            "memory_type": "semantic",
        }),
    )
    .await
    .expect("memory.remember");
    tokio::time::timeout(std::time::Duration::from_secs(10), warm_parked)
        .await
        .expect("the queued warm must reach its first attempt");

    assert_eq!(
        current_generation(&ann, &key).await,
        captured_generation + 1,
        "a non-replayed remember must bump the write generation exactly once"
    );
    assert_late_build_installs_below_the_counter(&ann, &key, captured_generation).await;

    ann.attempt_floor_barrier
        .store(false, std::sync::atomic::Ordering::SeqCst);
    ann.attempt_floor_release.notify_one();
    wait_until_warm_idle(&ann, &key).await;
}

/// `memory.prune` bumps the write generation of every registered model when it removes rows;
/// a build captured before that bump still installs afterwards.
///
/// The seeding write names no `embedding_model` for the reason given on the remember test,
/// and the runtime registers exactly one model, so the prune bumps exactly one counter.
/// No note-mutation hook is installed on this pack, so the bump observed here is the
/// handler's own. The warm queued by the seeding write is parked at its first attempt, so it
/// cannot install anything while the test reads the counter and installs the late build.
#[tokio::test]
#[serial(background_tasks)]
#[serial_test::serial(config_ledger)]
async fn prune_bumps_the_generation_and_a_prior_build_still_installs() {
    const MODEL: &str = "adr079-prune-bump-pin-model";
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    rt.register_embedder(HashVecProvider {
        model_name: MODEL.to_owned(),
        dims: 8,
    });
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    let pack = crate::MemoryPack::new(rt.clone());
    let ann = pack.ann_for_test();
    let key = AnnKey::from_token(MODEL);

    ann.attempt_floor_barrier
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let warm_parked = ann.attempt_floor_notify.notified();
    pack.handle_remember(
        &token,
        serde_json::json!({
            "content": "prune generation pin note",
            "memory_type": "semantic",
            "salience": 0.1,
        }),
    )
    .await
    .expect("seed memory.remember");
    tokio::time::timeout(std::time::Duration::from_secs(10), warm_parked)
        .await
        .expect("the queued warm must reach its first attempt");
    let captured_generation = current_generation(&ann, &key).await;

    let pruned = pack
        .handle_prune(
            &token,
            serde_json::json!({ "min_salience": 0.5, "namespace": "local" }),
        )
        .await
        .expect("memory.prune");

    assert_eq!(
        pruned["pruned"], 1,
        "the seeded note must be the one pruned: {pruned:?}"
    );
    assert_eq!(
        current_generation(&ann, &key).await,
        captured_generation + 1,
        "a prune that removed rows must bump the write generation exactly once"
    );
    assert_late_build_installs_below_the_counter(&ann, &key, captured_generation).await;

    ann.attempt_floor_barrier
        .store(false, std::sync::atomic::Ordering::SeqCst);
    ann.attempt_floor_release.notify_one();
    wait_until_warm_idle(&ann, &key).await;
}

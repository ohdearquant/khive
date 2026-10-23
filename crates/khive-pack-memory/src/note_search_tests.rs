use super::*;
use khive_pack_kg::KgPack;
use khive_runtime::pack::PackRuntime;
use khive_runtime::{BackendId, Namespace, RuntimeConfig, VerbRegistryBuilder};
use khive_storage::types::VectorSearchRequest;
use khive_types::SubstrateKind;
use serde_json::json;
use serial_test::serial;
use std::sync::Arc;

use crate::test_support::HashVecProvider;
use crate::MemoryPack;

const MODEL: &str = "all-minilm-l6-v2";
const DIMS: usize = 384;

fn vector(first: f32, second: f32) -> Vec<f32> {
    let mut value = vec![0.0; DIMS];
    value[0] = first;
    value[1] = second;
    value
}

fn runtime(path: &std::path::Path, backend_id: &str, fresh_tail: bool) -> KhiveRuntime {
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(path.to_owned()),
        backend_id: BackendId::parse(backend_id).expect("backend id"),
        embedding_model: Some(lattice_embed::EmbeddingModel::AllMiniLmL6V2),
        additional_embedding_models: vec![],
        ..RuntimeConfig::default()
    })
    .expect("runtime")
    .with_ann_fresh_tail_enabled(fresh_tail);
    rt.register_embedder(HashVecProvider {
        model_name: MODEL.to_owned(),
        dims: DIMS,
    });
    rt
}

async fn seed(rt: &KhiveRuntime, token: &NamespaceToken, text: &str) -> Uuid {
    rt.create_note(token, "memory", None, text, None, None, vec![])
        .await
        .expect("seed note")
        .id
}

async fn warm(rt: &KhiveRuntime, token: &NamespaceToken, pack: &MemoryPack) {
    ann::ensure_ann_for_model(rt, token, &pack.ann, MODEL)
        .await
        .expect("warm graph");
    assert!(
        ann::read_note_search_watermark(rt, MODEL)
            .await
            .expect("consumer row")
            .is_some_and(|watermark| watermark >= 0),
        "the note-search consumer must be active before the graph serves"
    );
}

#[tokio::test]
#[serial(note_search_ann)]
#[serial_test::serial(config_ledger)]
async fn note_search_visibility_filters_foreign_and_tombstoned_candidates() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rt = runtime(&dir.path().join("visibility.db"), "main", true);
    let local = rt.authorize(Namespace::local()).expect("local token");
    let foreign = rt
        .authorize(Namespace::parse("bench-arm-a").expect("foreign namespace"))
        .expect("foreign token");
    let pack = MemoryPack::new(rt.clone());
    let visible_id = seed(&rt, &local, "visible note").await;
    let foreign_id = seed(&rt, &foreign, "foreign note").await;
    let tombstoned_id = seed(&rt, &local, "deleted note").await;
    rt.delete_note(&local, tombstoned_id, false)
        .await
        .expect("soft delete");

    let provider = MemoryNoteSearchAnnProvider::new(rt.clone(), pack.ann.clone());
    let hits = provider
        .visible_hits(
            &local,
            vec![(visible_id, 0.9), (foreign_id, 1.0), (tombstoned_id, 1.0)],
        )
        .await
        .expect("visibility projection");
    assert!(hits.iter().any(|hit| hit.subject_id == visible_id));
    assert!(
        !hits.iter().any(|hit| hit.subject_id == foreign_id),
        "foreign namespace candidate leaked into local search"
    );
    assert!(
        !hits.iter().any(|hit| hit.subject_id == tombstoned_id),
        "soft-deleted candidate leaked into local search"
    );
}

/// MUST-FAIL: removing the fresh-tail merge loses this post-snapshot note
/// from the vector leg even though FTS would independently find its text.
#[tokio::test]
#[serial(note_search_ann)]
#[serial_test::serial(config_ledger)]
async fn note_search_ann_merges_post_snapshot_note_with_policy_disabled() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rt = runtime(&dir.path().join("tail.db"), "main", false);
    let token = rt.authorize(Namespace::local()).expect("local token");
    let pack = MemoryPack::new(rt.clone());
    pack.register_note_search_ann_provider(&rt);
    seed(&rt, &token, "graph snapshot seed").await;
    warm(&rt, &token, &pack).await;

    const FRESH: &str = "note search read your writes fresh marker";
    let fresh_id = seed(&rt, &token, FRESH).await;
    let query = rt.embed_query(FRESH).await.expect("query embedding");
    let provider = MemoryNoteSearchAnnProvider::new(rt.clone(), pack.ann.clone());
    let hits = provider
        .search(&token, MODEL, &query, 3)
        .await
        .expect("ANN plus exact tail")
        .expect("installed graph");
    assert!(
        hits.iter().any(|hit| hit.subject_id == fresh_id),
        "post-snapshot note must appear in the ANN vector leg: {hits:?}"
    );

    let (ann_before, fallback_before) = khive_runtime::note_search_ann::route_totals();
    let search_hits = rt
        .search_notes(
            &token,
            FRESH,
            Some(query),
            3,
            Some("memory"),
            false,
            &[],
            None,
        )
        .await
        .expect("search kind=note");
    assert!(search_hits.iter().any(|hit| hit.note_id == fresh_id));
    let (ann_after, fallback_after) = khive_runtime::note_search_ann::route_totals();
    assert_eq!(ann_after, ann_before + 1, "warm search must use ANN");
    assert_eq!(fallback_after, fallback_before, "warm search must not scan");
}

/// MUST-FAIL: a one-route distance conversion that clamps negative cosine
/// gives the opposite vector score zero instead of the exact route's -1.
#[tokio::test]
#[serial(note_search_ann)]
#[serial_test::serial(config_ledger)]
async fn note_search_ann_and_exact_route_rank_and_score_parity() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rt = runtime(&dir.path().join("ranking.db"), "main", true);
    let token = rt.authorize(Namespace::local()).expect("local token");
    let pack = MemoryPack::new(rt.clone());
    pack.register_note_search_ann_provider(&rt);
    let ids = [
        seed(&rt, &token, "positive direction").await,
        seed(&rt, &token, "orthogonal direction").await,
        seed(&rt, &token, "opposite direction").await,
    ];
    let vectors = [vector(1.0, 0.0), vector(0.0, 1.0), vector(-1.0, 0.0)];
    let store = rt.vectors_for_model(&token, MODEL).expect("vector store");
    for (id, vector) in ids.into_iter().zip(vectors) {
        store
            .insert(
                id,
                SubstrateKind::Note,
                "local",
                "note.content",
                vec![vector],
            )
            .await
            .expect("set deterministic vector");
    }
    warm(&rt, &token, &pack).await;
    let query = vector(1.0, 0.0);
    let provider = MemoryNoteSearchAnnProvider::new(rt.clone(), pack.ann.clone());
    let ann_hits = provider
        .search(&token, MODEL, &query, 3)
        .await
        .expect("ANN route")
        .expect("installed graph");
    let exact_hits = store
        .search(VectorSearchRequest {
            query_vectors: vec![query],
            top_k: 3,
            namespace: Some("local".to_owned()),
            kind: Some(SubstrateKind::Note),
            embedding_model: None,
            filter: None,
            backend_hints: None,
        })
        .await
        .expect("exact route");
    assert_eq!(
        ann_hits
            .iter()
            .map(|hit| (hit.subject_id, hit.score))
            .collect::<Vec<_>>(),
        exact_hits
            .iter()
            .map(|hit| (hit.subject_id, hit.score))
            .collect::<Vec<_>>(),
        "ANN and exact vector legs must agree in order and canonical score"
    );
    assert_eq!(ann_hits[2].score, DeterministicScore::from_f64(-1.0));
}

/// MUST-FAIL: an installed provider without a graph still selects the
/// exact fallback and increments only that route's process counter.
#[tokio::test]
#[serial(note_search_ann)]
#[serial_test::serial(config_ledger)]
async fn note_search_no_graph_uses_exact_fallback_counter() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rt = runtime(&dir.path().join("cold.db"), "main", true);
    let token = rt.authorize(Namespace::local()).expect("local token");
    let pack = MemoryPack::new_with_index_role(rt.clone(), false);
    pack.register_note_search_ann_provider(&rt);
    let id = seed(&rt, &token, "cold graph exact note").await;
    let query = rt
        .embed_query("cold graph exact note")
        .await
        .expect("query");
    let (ann_before, fallback_before) = khive_runtime::note_search_ann::route_totals();
    let hits = rt
        .search_notes(
            &token,
            "cold graph exact note",
            Some(query),
            3,
            Some("memory"),
            false,
            &[],
            None,
        )
        .await
        .expect("cold search");
    assert!(hits.iter().any(|hit| hit.note_id == id));
    let (ann_after, fallback_after) = khive_runtime::note_search_ann::route_totals();
    assert_eq!(ann_after, ann_before, "no graph must not count ANN");
    assert_eq!(
        fallback_after,
        fallback_before + 1,
        "no graph must count fallback"
    );
}

#[tokio::test]
#[serial(note_search_ann)]
#[serial_test::serial(config_ledger)]
async fn db_diagnostics_handler_reports_note_search_route_totals() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rt = runtime(&dir.path().join("route-diagnostics.db"), "main", true);
    let token = rt.authorize(Namespace::local()).expect("local token");
    let pack = MemoryPack::new(rt.clone());
    let ann = pack.ann.clone();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(pack);
    let registry = builder.build().expect("registry");
    let id = seed(&rt, &token, "route diagnostics note")
        .await
        .to_string();
    let search = json!({"kind": "note", "query": "route diagnostics note"});

    let (_, fallback_before) = khive_runtime::note_search_ann::route_totals();
    let cold = registry
        .dispatch("search", search.clone())
        .await
        .expect("cold note search");
    assert!(cold.as_array().is_some_and(|hits| hits
        .iter()
        .any(|hit| hit["id"].as_str() == Some(id.as_str())
            && hit["signals"]["vector_similarity"].is_number())));
    let (ann_after_cold, fallback_after_cold) = khive_runtime::note_search_ann::route_totals();
    assert!(fallback_after_cold > fallback_before);

    registry.call_register_note_search_ann_providers(&rt);
    ann::ensure_ann_for_model(&rt, &token, &ann, MODEL)
        .await
        .expect("warm note-search graph");
    assert!(ann::read_note_search_watermark(&rt, MODEL)
        .await
        .expect("consumer watermark")
        .is_some_and(|watermark| watermark >= 0));
    let hot = registry
        .dispatch("search", search)
        .await
        .expect("warm note search");
    assert!(hot.as_array().is_some_and(|hits| hits
        .iter()
        .any(|hit| hit["id"].as_str() == Some(id.as_str())
            && hit["signals"]["vector_similarity"].is_number())));
    let (ann_after_hot, fallback_after_hot) = khive_runtime::note_search_ann::route_totals();
    assert!(ann_after_hot > ann_after_cold);

    let report = registry
        .dispatch("db_diagnostics", json!({}))
        .await
        .expect("public db_diagnostics handler");
    let reported_ann = report["note_search_ann_route_total"]
        .as_u64()
        .expect("ANN route total");
    let reported_fallback = report["note_search_fallback_route_total"]
        .as_u64()
        .expect("fallback route total");
    assert!(reported_ann >= ann_after_hot);
    assert!(reported_fallback >= fallback_after_hot);
    assert!(report["databases"][0]["error"].is_null());
    assert_eq!(
        report["databases"][0]["diagnostics"]["note_search_ann_route_total"],
        reported_ann
    );
    assert_eq!(
        report["databases"][0]["diagnostics"]["note_search_fallback_route_total"],
        reported_fallback
    );
}

#[tokio::test]
#[serial(note_search_ann)]
#[serial_test::serial(config_ledger)]
async fn note_search_old_memory_graph_waits_for_its_own_full_checkpoint() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rt = runtime(&dir.path().join("old-graph.db"), "main", true);
    let token = rt.authorize(Namespace::local()).expect("local token");
    let pack = MemoryPack::new(rt.clone());
    let id = seed(&rt, &token, "pre-registration memory graph").await;
    ann::ensure_ann_for_model(&rt, &token, &pack.ann, MODEL)
        .await
        .expect("memory-only warm");
    assert_eq!(
        ann::read_note_search_watermark(&rt, MODEL)
            .await
            .expect("consumer row"),
        None,
        "memory-only warm must not silently register note search"
    );

    pack.register_note_search_ann_provider(&rt);
    let query = rt
        .embed_query("pre-registration memory graph")
        .await
        .expect("query");
    let provider = MemoryNoteSearchAnnProvider::new(rt.clone(), pack.ann.clone());
    assert!(
        provider
            .search(&token, MODEL, &query, 1)
            .await
            .expect("closed route")
            .is_none(),
        "a memory graph published before note_search registration cannot serve that consumer"
    );
    assert!(
        ann::search_loaded_with_seq(&pack.ann, &AnnKey::new(MODEL), &query, 1)
            .await
            .expect("memory bridge")
            .is_some(),
        "registering note search must not evict the memory consumer's protected graph"
    );
    let exact = rt
        .vector_search(&token, Some(query), None, 1, Some(SubstrateKind::Note))
        .await
        .expect("exact fallback");
    assert_eq!(exact[0].subject_id, id);
}

/// ADR-166 G3 mechanism guard: a file-backed warm note-search suite must
/// move the graph route counter without using the exact scan.
#[tokio::test]
#[serial(note_search_ann)]
#[serial_test::serial(config_ledger)]
async fn hot_path_guard_g3_warm_note_search_uses_ann_without_fallback() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rt = runtime(&dir.path().join("g3.db"), "main", true);
    let token = rt.authorize(Namespace::local()).expect("local token");
    let pack = MemoryPack::new(rt.clone());
    pack.register_note_search_ann_provider(&rt);
    seed(&rt, &token, "g3 warm note alpha").await;
    seed(&rt, &token, "g3 warm note beta").await;
    warm(&rt, &token, &pack).await;
    let query = rt.embed_query("g3 warm note").await.expect("query");

    let (ann_before, fallback_before) = khive_runtime::note_search_ann::route_totals();
    for _ in 0..3 {
        rt.search_notes(
            &token,
            "g3 warm note",
            Some(query.clone()),
            3,
            Some("memory"),
            false,
            &[],
            None,
        )
        .await
        .expect("warm search kind=note");
    }
    let (ann_after, fallback_after) = khive_runtime::note_search_ann::route_totals();
    assert_eq!(ann_after, ann_before + 3, "each warm query must use ANN");
    assert_eq!(
        fallback_after, fallback_before,
        "warm suite cannot silently scan"
    );
}

/// ADR-166 G5: count rows at the runtime's post-fusion hydration seam,
/// including the note-search ANN consumer's fresh-tail candidate.
#[tokio::test]
#[serial(note_search_ann)]
#[serial_test::serial(config_ledger)]
async fn hot_path_guard_g5_note_search_hydration_stays_within_candidate_window() {
    // Hold any newly enqueued rebuild at its existing test barrier so even a
    // fast worker cannot disappear before the per-model enqueue assertion.
    struct RebuildProbe(ann::SharedAnn);

    impl Drop for RebuildProbe {
        fn drop(&mut self) {
            self.0
                .attempt_floor_barrier
                .store(false, std::sync::atomic::Ordering::Release);
            self.0.attempt_floor_release.notify_one();
        }
    }

    async fn assert_no_rebuild_enqueued(shared: &ann::SharedAnn, key: &ann::AnnKey) {
        let idle = ann::wait_until_warm_idle(shared, key);
        tokio::pin!(idle);
        std::future::poll_fn(|cx| {
            assert!(
                std::future::Future::poll(idle.as_mut(), cx).is_ready(),
                "G5 fresh-tail path must not enqueue an ANN rebuild"
            );
            std::task::Poll::Ready(())
        })
        .await;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let rt = runtime(&dir.path().join("g5.db"), "main", true);
    let token = rt.authorize(Namespace::local()).expect("local token");
    let pack = MemoryPack::new(rt.clone());
    pack.register_note_search_ann_provider(&rt);
    for index in 0..40 {
        seed(&rt, &token, &format!("g5 candidate note {index}")).await;
    }
    warm(&rt, &token, &pack).await;
    let key = ann::AnnKey::new(MODEL);
    let rebuild_probe = RebuildProbe(pack.ann.clone());
    rebuild_probe
        .0
        .attempt_floor_barrier
        .store(true, std::sync::atomic::Ordering::Release);
    seed(&rt, &token, "g5 candidate note fresh tail").await;
    assert_no_rebuild_enqueued(&pack.ann, &key).await;
    let query = rt.embed_query("g5 candidate note").await.expect("query");
    let before = rt
        .db_diagnostics()
        .await
        .expect("diagnostics before search");

    let limit = 3;
    let hits = rt
        .search_notes(
            &token,
            "g5 candidate note",
            Some(query),
            limit,
            None,
            false,
            &[],
            None,
        )
        .await
        .expect("warm note search");
    assert_no_rebuild_enqueued(&pack.ann, &key).await;

    let after = rt.db_diagnostics().await.expect("diagnostics after search");
    let hydrated = after.search_mechanism.note_candidate_hydration_rows
        - before.search_mechanism.note_candidate_hydration_rows;
    assert_eq!(hits.len(), limit as usize);
    assert!(
        hydrated >= hits.len() as u64,
        "returned notes must have been hydrated: {hydrated}"
    );
    let per_arm = u64::from(limit) * 4;
    let fresh_tail = 1;
    assert!(
        hydrated <= per_arm * 2 + fresh_tail,
        "post-fusion hydration exceeded two bounded arms plus fresh tail: {hydrated}"
    );
}

#[tokio::test]
#[serial(note_search_ann)]
#[serial_test::serial(config_ledger)]
async fn note_search_ann_filters_foreign_and_tombstoned_neighbours() {
    let dir = tempfile::tempdir().expect("tempdir");
    let rt = runtime(&dir.path().join("scope.db"), "main", true);
    let local = rt.authorize(Namespace::local()).expect("local token");
    let foreign_ns = Namespace::parse("bench-arm-a").expect("foreign namespace");
    let foreign = rt.authorize(foreign_ns).expect("foreign token");
    let pack = MemoryPack::new(rt.clone());
    pack.register_note_search_ann_provider(&rt);
    let visible_id = seed(&rt, &local, "visible note").await;
    let foreign_id = seed(&rt, &foreign, "foreign near neighbour").await;
    let tombstoned_id = seed(&rt, &local, "deleted near neighbour").await;
    let store_local = rt.vectors_for_model(&local, MODEL).expect("local vectors");
    let store_foreign = rt
        .vectors_for_model(&foreign, MODEL)
        .expect("foreign vectors");
    for (store, id, ns, vector) in [
        (&store_local, visible_id, "local", vector(0.9, 0.1)),
        (&store_foreign, foreign_id, "bench-arm-a", vector(1.0, 0.0)),
        (&store_local, tombstoned_id, "local", vector(1.0, 0.0)),
    ] {
        store
            .insert(id, SubstrateKind::Note, ns, "note.content", vec![vector])
            .await
            .expect("set near-neighbour vector");
    }
    warm(&rt, &local, &pack).await;
    rt.delete_note(&local, tombstoned_id, false)
        .await
        .expect("soft delete");
    let provider = MemoryNoteSearchAnnProvider::new(rt.clone(), pack.ann.clone());
    let query = vector(1.0, 0.0);
    let ann_hits = provider
        .search(&local, MODEL, &query, 3)
        .await
        .expect("ANN")
        .expect("graph");
    let exact_hits = store_local
        .search(VectorSearchRequest {
            query_vectors: vec![query],
            top_k: 3,
            namespace: Some("local".into()),
            kind: Some(SubstrateKind::Note),
            embedding_model: None,
            filter: None,
            backend_hints: None,
        })
        .await
        .expect("exact");
    assert_eq!(
        ann_hits
            .iter()
            .map(|hit| hit.subject_id)
            .collect::<Vec<_>>(),
        exact_hits
            .iter()
            .map(|hit| hit.subject_id)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        ann_hits
            .iter()
            .map(|hit| hit.subject_id)
            .collect::<Vec<_>>(),
        vec![visible_id]
    );
    assert!(!ann_hits
        .iter()
        .any(|hit| hit.subject_id == foreign_id || hit.subject_id == tombstoned_id));
}

#[tokio::test]
#[serial(note_search_ann)]
#[serial_test::serial(config_ledger)]
async fn note_search_mismatched_backend_registration_keeps_fallback() {
    let dir = tempfile::tempdir().expect("tempdir");
    let memory_rt = runtime(&dir.path().join("memory.db"), "memory-routed", true);
    let query_rt = runtime(&dir.path().join("query.db"), "main", true);
    let pack = MemoryPack::new(memory_rt);
    pack.register_note_search_ann_provider(&query_rt);
    let token = query_rt.authorize(Namespace::local()).expect("local token");
    let id = seed(&query_rt, &token, "routed mismatch note").await;
    let query = query_rt
        .embed_query("routed mismatch note")
        .await
        .expect("query");
    let (ann_before, fallback_before) = khive_runtime::note_search_ann::route_totals();
    let hits = query_rt
        .search_notes(
            &token,
            "routed mismatch note",
            Some(query),
            3,
            Some("memory"),
            false,
            &[],
            None,
        )
        .await
        .expect("exact routed search");
    assert!(hits.iter().any(|hit| hit.note_id == id));
    let (ann_after, fallback_after) = khive_runtime::note_search_ann::route_totals();
    assert_eq!(ann_after, ann_before);
    assert_eq!(fallback_after, fallback_before + 1);
}

#[tokio::test]
#[serial(note_search_ann)]
#[serial_test::serial(config_ledger)]
async fn note_search_same_backend_name_different_store_keeps_fallback() {
    let dir = tempfile::tempdir().expect("tempdir");
    let memory_rt = runtime(&dir.path().join("memory-main.db"), "main", true);
    let query_rt = runtime(&dir.path().join("query-main.db"), "main", true);
    assert!(!std::ptr::eq(memory_rt.backend(), query_rt.backend()));
    let pack = MemoryPack::new(memory_rt.clone());
    pack.register_note_search_ann_provider(&memory_rt);
    let memory_token = memory_rt
        .authorize(Namespace::local())
        .expect("memory token");
    seed(&memory_rt, &memory_token, "foreign physical store note").await;
    warm(&memory_rt, &memory_token, &pack).await;
    pack.register_note_search_ann_provider(&query_rt);
    // The public install seam enforces the same physical binding even if
    // a caller bypasses the pack registration hook.
    query_rt.install_note_search_ann_provider(Arc::new(MemoryNoteSearchAnnProvider::new(
        memory_rt,
        pack.ann.clone(),
    )));
    let token = query_rt.authorize(Namespace::local()).expect("local token");
    let id = seed(&query_rt, &token, "same-name different-store note").await;
    let query = query_rt
        .embed_query("same-name different-store note")
        .await
        .expect("query");
    let (ann_before, fallback_before) = khive_runtime::note_search_ann::route_totals();
    let hits = query_rt
        .search_notes(
            &token,
            "same-name different-store note",
            Some(query),
            3,
            Some("memory"),
            false,
            &[],
            None,
        )
        .await
        .expect("exact same-name search");
    assert!(hits.iter().any(|hit| hit.note_id == id));
    let (ann_after, fallback_after) = khive_runtime::note_search_ann::route_totals();
    assert_eq!(ann_after, ann_before);
    assert_eq!(fallback_after, fallback_before + 1);
}

#[test]
fn note_search_provider_drops_after_owning_runtime_drops() {
    let dir = tempfile::tempdir().expect("tempdir");
    let weak = {
        let rt = runtime(&dir.path().join("lifecycle.db"), "main", true);
        let provider: Arc<dyn NoteSearchAnnProvider> = Arc::new(MemoryNoteSearchAnnProvider::new(
            rt.clone(),
            ann::new_shared_for_role(false),
        ));
        let weak = Arc::downgrade(&provider);
        rt.install_note_search_ann_provider(provider);
        assert!(weak.upgrade().is_some(), "provider should be installed");
        weak
    };
    assert!(
        weak.upgrade().is_none(),
        "the provider must not retain its own runtime installation slot"
    );
}

#[test]
fn note_search_provider_and_mutation_hook_release_ann_after_runtime_drops() {
    let dir = tempfile::tempdir().expect("tempdir");
    let weak_ann = {
        let rt = runtime(&dir.path().join("composed-lifecycle.db"), "main", true);
        let pack = MemoryPack::new_with_index_role(rt.clone(), false);
        let weak_ann = Arc::downgrade(&pack.ann);
        pack.register_note_mutation_hook(&rt);
        pack.register_note_search_ann_provider(&rt);
        assert!(weak_ann.upgrade().is_some(), "ANN should be installed");
        weak_ann
    };
    assert!(
        weak_ann.upgrade().is_none(),
        "the mutation hook must not retain the runtime and its note-search provider"
    );
}

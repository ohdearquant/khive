use super::*;

#[test]
fn stage_timings_distinguish_skips_from_sub_millisecond_work() {
    let skipped = RecallStageTimings::default();
    let mut measured = RecallStageTimings::default();
    measured.record_parallel_model(Duration::from_micros(500), Some(Duration::ZERO));
    measured.add_hydration(Duration::from_micros(500));
    assert_eq!(skipped.ann_ms(), measured.ann_ms());
    assert!(!skipped.ann_attempted());
    assert!(measured.ann_attempted());
    assert!(measured.fresh_tail_attempted());
    assert_eq!(measured.fresh_tail_ms(), 0);
    assert!(measured.hydrate_attempted());
    assert_eq!(measured.hydrate_ms(), 0);

    let mut total = RecallStageTimings::default();
    total.add_retrieval_round(skipped);
    assert!(!total.ann_attempted());
    total.add_retrieval_round(measured);
    total.add_retrieval_round(skipped);
    total.record_parallel_model(Duration::ZERO, None);
    assert!(total.ann_attempted());
    assert!(
        total.fresh_tail_attempted(),
        "later skipped work cannot erase a measurement"
    );
    assert!(!total.embed_attempted());
    assert!(!total.fts_attempted());
    assert!(
        !total.hydrate_attempted(),
        "hydration is accumulated separately"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn no_model_recall_records_fts_without_inventing_vector_stage_work() {
    let rt = khive_runtime::KhiveRuntime::memory().unwrap();
    assert!(rt.registered_embedding_model_names().is_empty());
    let token = rt.authorize(khive_runtime::Namespace::local()).unwrap();
    let pack = MemoryPack::new(rt);
    let result = pack
        .collect_recall_candidates(
            "stage telemetry",
            &token,
            RecallCandidateParams {
                candidate_limit: 10,
                legs: RecallLegs::BOTH,
                embedding_model: None,
                session_fence: None,
                cjk_fts_bypass: false,
                snippet_policy: TextSnippetPolicy::Omit,
                fts_gather: &crate::config::RecallFtsGatherConfig::default(),
                ann_overfetch_max_rounds: 1,
                ann_ready_timeout_ms: 10,
            },
        )
        .await
        .unwrap();
    assert!(result.vector_hits_per_model.is_empty());
    assert!(result.timings.fts_attempted());
    assert!(!result.timings.embed_attempted());
    assert!(!result.timings.ann_attempted());
    assert!(!result.timings.fresh_tail_attempted());
    assert!(!result.timings.hydrate_attempted());
}

#[tokio::test]
async fn exact_sqlite_vec_recall_records_ann_but_skips_fresh_tail() {
    use crate::test_support::HashVecProvider;
    use khive_runtime::{KhiveRuntime, RuntimeConfig};

    const MODEL: &str = "stage-timing-exact-model";
    let dir = tempfile::tempdir().unwrap();
    let cfg = RuntimeConfig {
        db_path: Some(dir.path().join("recall-stage.db")),
        ..RuntimeConfig::no_embeddings()
    };
    let writable = KhiveRuntime::new(cfg.clone()).unwrap();
    writable.register_embedder(HashVecProvider {
        model_name: MODEL.into(),
        dims: 8,
    });
    let token = writable
        .authorize(khive_runtime::Namespace::local())
        .unwrap();
    writable.vectors_for_model(&token, MODEL).unwrap();
    let writer_join = writable.backend().pool().take_writer_task_join();
    drop(token);
    drop(writable);
    if let Some(join) = writer_join {
        tokio::time::timeout(Duration::from_secs(5), join)
            .await
            .expect("fixture writer must drain before readonly reopen")
            .expect("fixture writer must not panic");
    }
    let rt = KhiveRuntime::new_readonly(cfg).unwrap();
    rt.register_embedder(HashVecProvider {
        model_name: MODEL.into(),
        dims: 8,
    });
    let token = rt.authorize(khive_runtime::Namespace::local()).unwrap();
    let shared = ann::new_shared();
    let result = collect_model_ann_hits(
        &rt,
        &shared,
        &token,
        "local",
        &["local".into()],
        MODEL.into(),
        vec![1.0; 8],
        10,
        40,
        1,
        10,
        None,
    )
    .await
    .unwrap();
    assert!(
        result.used_sqlite_vec_fallback,
        "must exercise the exact route"
    );
    assert!(
        !result.degraded,
        "a failed exact query is not a successful control"
    );
    assert!(result.fresh_tail_elapsed.is_none());
    let mut timings = RecallStageTimings::default();
    timings.record_parallel_model(result.ann_elapsed, result.fresh_tail_elapsed);
    assert!(timings.ann_attempted());
    assert!(!timings.fresh_tail_attempted());
    assert_eq!(timings.fresh_tail_ms(), 0);
}

#[test]
fn stage_timings_max_parallel_models_and_sum_sequential_rounds() {
    let mut first_round = RecallStageTimings::default();
    first_round.record_parallel_model(Duration::from_millis(13), Some(Duration::from_millis(2)));
    first_round.record_parallel_model(Duration::from_millis(5), Some(Duration::from_millis(11)));
    assert_eq!(first_round.ann_ms(), 13);
    assert_eq!(first_round.fresh_tail_ms(), 11);

    let second_round = RecallStageTimings::from_millis_for_test(3, 7, 17, 19, 0);
    let mut total = RecallStageTimings::default();
    total.add_retrieval_round(first_round);
    total.add_retrieval_round(second_round);
    total.add_hydration(Duration::from_millis(23));

    assert_eq!(total.embed_ms(), 3);
    assert_eq!(total.fts_ms(), 7);
    assert_eq!(total.ann_ms(), 30);
    assert_eq!(total.fresh_tail_ms(), 30);
    assert_eq!(total.hydrate_ms(), 23);
}

#[tokio::test]
async fn cancelled_embedding_failure_cannot_degrade_to_a_healthy_engine() {
    let (_cancel_tx, cancel_rx) = tokio::sync::watch::channel(true);
    let result = khive_storage::scope_request_read_cancellation(cancel_rx, async {
        collect_embed_results(vec![
            (
                "failed".to_string(),
                Err(RuntimeError::Internal(
                    "ordinary engine failure".to_string(),
                )),
            ),
            (
                "healthy".to_string(),
                Ok(("healthy".to_string(), vec![1.0, 0.0])),
            ),
        ])
    })
    .await;

    assert!(matches!(
        result,
        Err(RuntimeError::Storage(
            khive_storage::error::StorageError::Timeout { .. }
        ))
    ));
}

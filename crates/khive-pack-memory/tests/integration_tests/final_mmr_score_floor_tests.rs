#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn recall_score_floor_applies_to_final_mmr_rank_score() {
    let config = RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: Vec::new(),
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: khive_runtime::WalCeilingSource::Default,
        wal_ceiling_env_raw: None,
        disk_guard_config: None,
        volume_lock_dir: None,
        actor_id: None,
        brain_profile: None,
        brain: Default::default(),
        visibility_receipts: None,
        credentials: Vec::new(),
        mounts: Vec::new(),
        events_split: None,
        blob: Default::default(),
        packs: vec!["kg".into(), "memory".into()],
        ..RuntimeConfig::no_embeddings()
    };
    assert!(config.db_path.is_none());
    assert!(config.embedding_model.is_none());
    assert!(config.additional_embedding_models.is_empty());
    let rt = KhiveRuntime::new(config).expect("in-memory runtime");
    assert!(!rt.backend().is_file_backed());
    assert!(rt.backend_data_dir().is_none());
    assert!(rt.backend_ann_root().is_none());
    assert!(rt.registered_embedding_model_names().is_empty());
    let registry = make_registry(rt);
    assert!(!registry.has_verb("brain.resolve"));

    const DUPLICATE: &str = concat!(
        "recallfloor duplicate prefix shared by two semantic memories for the final ",
        "composite threshold regression; both copies must reach the real text search."
    );
    let mut ids = Vec::new();
    for (content, salience) in [
        (DUPLICATE, 1.0),
        (DUPLICATE, 0.75),
        (
            "Boundary recallfloor semantic memory with a distinct prefix",
            0.625,
        ),
        (
            "Below recallfloor semantic memory with another distinct prefix",
            0.25,
        ),
    ] {
        let created = registry
            .dispatch(
                "memory.remember",
                json!({
                    "content": content,
                    "memory_type": "semantic",
                    "salience": salience,
                    "decay_factor": 0.0,
                }),
            )
            .await
            .expect("remember through the real registry");
        assert_eq!(created["salience"], json!(salience));
        ids.push(created["id"].as_str().expect("full memory id").to_owned());
    }
    assert_ne!(ids[0], ids[1], "unkeyed duplicate writes remain distinct");

    let params = |penalty| {
        json!({
            "query": "recallfloor",
            "fusion_strategy": "keyword_only",
            "entity_names": [],
            "limit": 10,
            "config": {
                "decay_model": "none",
                "reranker_weights": {"salience": 1.0},
                "candidate_limit": 20,
                "ann_overfetch_max_rounds": 1,
                "scoring": {"mmr_penalty": penalty, "mmr_prefix_len": 100},
            },
        })
    };
    let assert_hits = |value: &Value, expected: &[(usize, f64)]| {
        let hits = value.as_array().expect("text-only recall returns an array");
        assert_eq!(hits.len(), expected.len(), "{value}");
        for (hit, &(index, rank_score)) in hits.iter().zip(expected) {
            assert_eq!(hit["full_id"].as_str(), Some(ids[index].as_str()));
            assert_eq!(hit["rank_score"].as_f64(), Some(rank_score));
            assert!(hit["raw_score"].is_null(), "FTS-only fixture: {hit}");
        }
    };

    let baseline = registry
        .dispatch("memory.recall", params(0.25))
        .await
        .expect("zero-default floor recall");
    assert_hits(&baseline, &[(0, 1.0), (2, 0.625), (1, 0.5), (3, 0.25)]);
    assert_eq!(baseline[2]["score"].as_f64(), Some(0.75));

    let mut no_mmr = params(0.0);
    no_mmr["min_score"] = json!(0.625);
    let without_penalty = registry.dispatch("memory.recall", no_mmr).await.unwrap();
    assert_hits(&without_penalty, &[(0, 1.0), (1, 0.75), (2, 0.625)]);

    for field in ["min_score", "score_floor", "config.min_score"] {
        let mut request = params(0.25);
        if field == "config.min_score" {
            request["config"]["min_score"] = json!(0.625);
        } else {
            request[field] = json!(0.625);
        }
        let result = registry.dispatch("memory.recall", request).await.unwrap();
        assert_hits(&result, &[(0, 1.0), (2, 0.625)]);
        for hit in result.as_array().unwrap() {
            assert!(
                hit["rank_score"].as_f64().unwrap() >= 0.625,
                "{field}: {hit}"
            );
        }
    }

    let mut zero_floor = params(0.25);
    zero_floor["score_floor"] = json!(0.0);
    let zero = registry
        .dispatch("memory.recall", zero_floor)
        .await
        .unwrap();
    assert_hits(&zero, &[(0, 1.0), (2, 0.625), (1, 0.5), (3, 0.25)]);
}

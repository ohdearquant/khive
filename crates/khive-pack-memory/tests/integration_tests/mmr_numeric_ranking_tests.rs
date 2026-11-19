/// Identical content ties at retrieval. The MMR keeper must be selected by the
/// composite score even when the higher-salience memory has the larger UUID.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn default_mmr_keeps_higher_composite_duplicate_in_both_id_orders() {
    const CONTENT: &str = "Persistent semantic memory about reliable ranking of identical research observations across sessions, where salience must select the representative before a near duplicate penalty changes the displayed order.";

    for high_id_is_lower in [true, false] {
        let rt = make_runtime();
        let registry = make_registry(rt.clone());
        let mut ids = Vec::new();
        for _ in 0..2 {
            let remembered = registry
                .dispatch(
                    "memory.remember",
                    json!({
                        "content": CONTENT,
                        "memory_type": "semantic",
                        "salience": 0.5,
                        "decay": 0.0
                    }),
                )
                .await
                .expect("create identical semantic memory");
            ids.push(Uuid::parse_str(remembered["id"].as_str().unwrap()).unwrap());
        }
        ids.sort();
        let (high_id, low_id) = if high_id_is_lower {
            (ids[0], ids[1])
        } else {
            (ids[1], ids[0])
        };

        // Assign salience after IDs exist so both UUID tie-break directions
        // are exercised instead of relying on the generator's order.
        let sql = rt.sql();
        let mut writer = sql.writer().await.expect("sql writer");
        for (id, salience) in [(high_id, 1.0), (low_id, 0.0)] {
            assert_eq!(
                writer
                    .execute(SqlStatement {
                        sql: "UPDATE notes SET salience = ? WHERE id = ?".into(),
                        params: vec![SqlValue::Float(salience), SqlValue::Text(id.to_string())],
                        label: Some("test-mmr-salience-order".into()),
                    })
                    .await
                    .expect("set valid salience"),
                1
            );
        }
        drop(writer);

        let unpenalized = registry
            .dispatch(
                "memory.recall",
                json!({
                    "query": CONTENT,
                    "limit": 2,
                    // Fusion ranks can differ even for byte-identical text. Make
                    // salience decisive so this tests MMR keeper selection, not
                    // whichever UUID happened to win a retrieval tie.
                    "config": { "scoring": {
                        "weights": { "salience": 8.0 },
                        "mmr_penalty": 0.0
                    } }
                }),
            )
            .await
            .expect("unpenalized control recall");
        let control_hits = unpenalized.as_array().expect("control hits");
        assert_eq!(control_hits.len(), 2);
        let high_id_text = high_id.to_string();
        let low_id_text = low_id.to_string();
        let control_score = |id: &str| {
            control_hits
                .iter()
                .find(|hit| hit["id"].as_str() == Some(id))
                .expect("control hit by ID")["rank_score"]
                .as_f64()
                .expect("control rank_score")
        };
        let high_control_score = control_score(&high_id_text);
        let low_control_score = control_score(&low_id_text);
        assert!(
            high_control_score > low_control_score,
            "fixture must give the high-salience copy a higher composite score"
        );
        assert!(
            low_control_score > 0.1,
            "fixture must keep the lower-scoring duplicate above the default MMR penalty"
        );

        let default = registry
            .dispatch(
                "memory.recall",
                json!({
                    "query": CONTENT,
                    "limit": 2,
                    "config": { "scoring": { "weights": { "salience": 8.0 } } }
                }),
            )
            .await
            .expect("default MMR recall");
        let hits = default.as_array().expect("default hits");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0]["id"].as_str(), Some(high_id_text.as_str()));
        let high_final_score = hits[0]["rank_score"].as_f64().expect("rank_score");
        let low_final_score = hits
            .iter()
            .find(|hit| hit["id"].as_str() == Some(low_id_text.as_str()))
            .expect("lower-scoring duplicate in results")["rank_score"]
            .as_f64()
            .expect("lower-scoring duplicate rank_score");
        assert!(
            (high_final_score - high_control_score).abs() < 1e-3,
            "higher composite duplicate must not receive MMR penalty: \
             high_id_is_lower={high_id_is_lower}, control={high_control_score}, final={high_final_score}"
        );
        assert!(
            (low_final_score - (low_control_score - 0.1)).abs() < 1e-3,
            "lower composite duplicate must receive the default MMR penalty: \
             high_id_is_lower={high_id_is_lower}, control={low_control_score}, final={low_final_score}"
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn finite_adjustments_keep_rank_scores_numeric_through_mmr_and_final_sort() {
    let rt = make_runtime();
    rt.register_embedder(ConstVecProvider::new("finite-adjustment-enc", 4, 0.9));
    let registry = make_registry(rt.clone());

    let mut episodic_id = String::new();
    for (content, memory_type) in [
        ("Alpha rank score arithmetic memory", "episodic"),
        ("Beta rank score arithmetic memory", "semantic"),
    ] {
        let remembered = registry
            .dispatch(
                "memory.remember",
                json!({
                    "content": content,
                    "memory_type": memory_type,
                    "salience": 0.8
                }),
            )
            .await
            .expect("remember memory with vector embedding");
        if memory_type == "episodic" {
            episodic_id = remembered["id"].as_str().expect("episodic ID").to_owned();
        }
    }

    let pack = MemoryPack::new(receipt_credentials::with_receipt_credentials(rt.clone()));
    pack.warm().await;
    let token = rt.authorize(Namespace::local()).expect("local token");

    // A vector hit supplies raw_score, so the scored candidate reaches both sorts.
    let control = pack
        .dispatch(
            "memory.recall",
            json!({
                "query": "rank score arithmetic",
                "limit": 2,
                "config": {"scoring": {
                    "mmr_penalty": 0.1,
                    "mmr_prefix_len": 1,
                    "adjustments": []
                }}
            }),
            &registry,
            &token,
        )
        .await
        .expect("control recall");
    let control_hits = control.as_array().expect("control hits");
    assert_eq!(control_hits.len(), 2, "control must retrieve both memories");
    assert!(
        control_hits
            .iter()
            .all(|hit| hit["raw_score"].as_f64().is_some()),
        "both candidates must have vector raw scores: {control_hits:?}"
    );

    let episodic_condition = json!({"type": "memory_type", "kind": "episodic"});
    let config = json!({"scoring": {
        "mmr_penalty": 0.1,
        "mmr_prefix_len": 1,
        "adjustments": [
            {"condition": episodic_condition, "operation": {"type": "add", "value": 3.0e38_f32}},
            {"condition": episodic_condition, "operation": {"type": "add", "value": 3.0e38_f32}},
            {"condition": episodic_condition, "operation": {"type": "multiply", "factor": 0.0}}
        ]
    }});
    khive_pack_memory::config::RecallConfig::try_from_value(config.clone())
        .expect("finite nested adjustments are accepted by RecallConfig");

    let recalled = pack
        .dispatch(
            "memory.recall",
            json!({"query": "rank score arithmetic", "limit": 2, "config": config}),
            &registry,
            &token,
        )
        .await
        .expect("recall with finite nested adjustments");
    let hits = recalled.as_array().expect("recall hits");
    assert_eq!(hits.len(), 2, "both memories must reach the scored results");
    let episodic = hits
        .iter()
        .find(|hit| hit["id"].as_str() == Some(episodic_id.as_str()))
        .expect("episodic memory in results");
    assert!(
        episodic["rank_score"]
            .as_f64()
            .is_some_and(|score| score.is_finite()),
        "finite accepted adjustments must yield a numeric rank_score: {episodic:?}"
    );
}

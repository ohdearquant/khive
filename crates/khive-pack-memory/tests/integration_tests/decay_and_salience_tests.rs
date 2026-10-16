#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_recall_decay_ranking() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    // Both notes have IDENTICAL content so BM25 assigns equal relevance scores.
    // The only difference is creation time and decay_factor, so temporal decay
    // must determine the ranking. This makes the test independent of BM25 tie-breaking.
    let shared_content = "memory about neural networks and deep learning";

    let fresh = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": shared_content,
                "salience": 0.7,
                "decay": 0.01
            }),
        )
        .await
        .expect("fresh remember");
    let fresh_id = fresh["id"].as_str().unwrap().to_string();

    // Create old memory (simulate 90 days ago) with high decay
    let old = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": shared_content,
                "salience": 0.7,
                "decay": 0.1
            }),
        )
        .await
        .expect("old remember");
    let old_id = old["id"].as_str().unwrap().to_string();

    // Manually backdate the old note to simulate age. `upsert_note` preserves
    // `created_at` on existing rows, so the backdate must be a direct UPDATE.
    let old_uuid: uuid::Uuid = old_id.parse().unwrap();
    let sql = rt.sql();
    let mut writer = sql.writer().await.expect("sql writer");
    let backdated = writer
        .execute(SqlStatement {
            sql: "UPDATE notes SET created_at = created_at - ? WHERE id = ?".into(),
            params: vec![
                SqlValue::Integer(90 * 86_400_000_000i64),
                SqlValue::Text(old_uuid.to_string()),
            ],
            label: Some("test-backdate-note".into()),
        })
        .await
        .expect("backdate update");
    assert_eq!(backdated, 1, "backdate must touch exactly the old note row");
    drop(writer);

    // Disable MMR penalty so this assertion isolates temporal decay from the
    // separate near-duplicate adjustment.
    let recall_result = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": "neural networks deep learning",
                "config": {
                    "scoring": {
                        "mmr_penalty": 0.0
                    }
                }
            }),
        )
        .await
        .expect("recall succeeds");

    let hits = recall_result.as_array().expect("array");
    let ranks: Vec<(&str, f64)> = hits
        .iter()
        .map(|h| {
            (
                h["id"].as_str().unwrap(),
                h["rank_score"].as_f64().unwrap_or(0.0),
            )
        })
        .collect();
    let fresh_entry = ranks
        .iter()
        .find(|(id, _)| *id == fresh_id)
        .expect("fresh in results");
    let old_entry = ranks
        .iter()
        .find(|(id, _)| *id == old_id)
        .expect("old in results");
    assert!(
        fresh_entry.1 > old_entry.1,
        "fresh memory (rank_score={}) should rank higher than 90-day-old high-decay memory (rank_score={})",
        fresh_entry.1,
        old_entry.1
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_recall_salience_ranking() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    // Use non-identical content so this assertion isolates salience from the
    // separate near-duplicate adjustment.
    let high = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "high-salience concept about knowledge representation theory",
                "salience": 0.9,
                "decay": 0.0
            }),
        )
        .await
        .expect("high salience remember");
    let high_id = high["id"].as_str().unwrap().to_string();

    let low = registry
        .dispatch(
            "memory.remember",
            json!({
                "content": "low-salience concept about knowledge representation systems",
                "salience": 0.1,
                "decay": 0.0
            }),
        )
        .await
        .expect("low salience remember");
    let low_id = low["id"].as_str().unwrap().to_string();

    let recall_result = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "knowledge representation" }),
        )
        .await
        .expect("recall succeeds");

    let hits = recall_result.as_array().expect("array");
    let ranks: Vec<(&str, f64)> = hits
        .iter()
        .map(|h| {
            (
                h["id"].as_str().unwrap(),
                h["rank_score"].as_f64().unwrap_or(0.0),
            )
        })
        .collect();
    let high_entry = ranks
        .iter()
        .find(|(id, _)| *id == high_id)
        .expect("high in results");
    let low_entry = ranks
        .iter()
        .find(|(id, _)| *id == low_id)
        .expect("low in results");
    assert!(
        high_entry.1 >= low_entry.1,
        "high salience memory (rank_score={}) should rank >= low salience (rank_score={})",
        high_entry.1,
        low_entry.1
    );
}

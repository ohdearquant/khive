/// High inbound fan-in on one candidate must not hide another target. The
/// same suppression fixture also observes and plans the actual recall SQL.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_recall_supersedes_suppression_exhausts_matching_edges() {
    let rt = make_runtime();
    let token = rt
        .authorize(Namespace::parse("local").expect("local namespace"))
        .expect("authorize local");
    let query = "issue seventeen forty nine exhaustive supersedes target";

    let target_a = rt
        .create_note(
            &token,
            "memory",
            None,
            &format!("{query} alpha"),
            Some(0.8),
            Some(json!({ "memory_type": "semantic" })),
            vec![],
        )
        .await
        .expect("create target A");
    let target_b = rt
        .create_note(
            &token,
            "memory",
            None,
            &format!("{query} beta"),
            Some(0.8),
            Some(json!({ "memory_type": "semantic" })),
            vec![],
        )
        .await
        .expect("create target B");
    let target_a_id = target_a.id.to_string();
    let target_b_id = target_b.id.to_string();

    // Ensure graph DDL (including the immutable insertion-sequence trigger) is
    // installed, then seed the cardinality fixture directly. The source IDs do
    // not participate in recall suppression; making them unique preserves the
    // graph edge natural key while avoiding hundreds of unrelated note writes.
    let _graph = rt.graph(&token).expect("graph store");
    const TARGET_A_EDGE_COUNT: usize = 300;
    let mut a_rows = Vec::with_capacity(TARGET_A_EDGE_COUNT);
    let mut a_params = Vec::with_capacity(TARGET_A_EDGE_COUNT * 3);
    for _ in 0..TARGET_A_EDGE_COUNT {
        a_rows.push("('local', ?, ?, ?, 'supersedes', 1.0, 2, 2)".to_string());
        a_params.extend([
            SqlValue::Text(Uuid::new_v4().to_string()),
            SqlValue::Text(Uuid::new_v4().to_string()),
            SqlValue::Text(target_a_id.clone()),
        ]);
    }

    let mut writer = rt.sql().writer().await.expect("sql writer");
    let inserted_a = writer
        .execute(SqlStatement {
            sql: format!(
                "INSERT INTO graph_edges \
                 (namespace, id, source_id, target_id, relation, weight, created_at, updated_at) \
                 VALUES {}",
                a_rows.join(",")
            ),
            params: a_params,
            label: Some("test-1749-seed-first-supersedes-page".into()),
        })
        .await
        .expect("seed full target-A edge page");
    assert_eq!(
        inserted_a,
        u64::try_from(TARGET_A_EDGE_COUNT).expect("fixture edge count fits u64"),
        "fixture must exceed one supersedes page"
    );

    // Insert B after all A edges in sequence order, but give it an older
    // created_at. The fixed seek walk needs a later page to see B; the reverted
    // created_at-desc query with limit=candidate_count sees only A.
    let inserted_b = writer
        .execute(SqlStatement {
            sql: "INSERT INTO graph_edges \
                  (namespace, id, source_id, target_id, relation, weight, created_at, updated_at) \
                  VALUES ('local', ?1, ?2, ?3, 'supersedes', 1.0, 1, 1)"
                .into(),
            params: vec![
                SqlValue::Text(Uuid::new_v4().to_string()),
                SqlValue::Text(Uuid::new_v4().to_string()),
                SqlValue::Text(target_b_id.clone()),
            ],
            label: Some("test-1749-seed-second-supersedes-page".into()),
        })
        .await
        .expect("seed target-B edge after first page");
    assert_eq!(inserted_b, 1, "fixture must add the page-two edge");
    drop(writer);

    let registry = make_registry(rt.clone());
    let baseline = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": query,
                "limit": 2,
                "fusion_strategy": "keyword_only",
                "config": {
                    "scoring": {
                        "enable_supersedes_suppression": false,
                        "mmr_penalty": 0.0
                    }
                }
            }),
        )
        .await
        .expect("baseline recall");
    let baseline_ids: Vec<&str> = baseline
        .as_array()
        .expect("baseline is an array")
        .iter()
        .filter_map(|hit| hit["id"].as_str())
        .collect();
    assert_eq!(baseline_ids.len(), 2, "both target memories must rank");
    assert!(baseline_ids.contains(&target_a_id.as_str()));
    assert!(baseline_ids.contains(&target_b_id.as_str()));

    let observation = rt
        .core()
        .backend()
        .pool()
        .observe_test_statement_starts(1024)
        .expect("observe actual recall statements");
    let suppressed = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": query,
                "limit": 2,
                "fusion_strategy": "keyword_only",
                "config": {
                    "scoring": {
                        "enable_supersedes_suppression": true,
                        "mmr_penalty": 0.0
                    }
                }
            }),
        )
        .await
        .expect("suppressed recall");
    assert!(
        suppressed.as_array().is_some_and(Vec::is_empty),
        "every targeted candidate must be suppressed after target lookups: {suppressed}"
    );
    let statements = observation
        .started_statements()
        .expect("complete statement observation");
    drop(observation);
    let edge_lookups: Vec<_> = statements
        .iter()
        .filter(|statement| {
            statement
                .sql
                .contains("edges.target_id = requested.origin_id")
                && statement.sql.contains("edges.relation IN (")
        })
        .collect();
    assert_eq!(
        edge_lookups.len(),
        1,
        "one batched lookup covers both candidates"
    );
    let mut reader = rt.sql().reader().await.expect("planner reader");
    for statement in edge_lookups {
        assert!(statement.readonly);
        assert!(
            !statement.sql.contains("graph_edges_seq"),
            "SUPERSEDES_TARGET_LOOKUP: recall must not drive the global insertion ledger: {}",
            statement.sql
        );
        let plan = reader
            .explain(SqlStatement {
                sql: statement.sql.clone(),
                params: vec![
                    SqlValue::Text("local".into()),
                    SqlValue::Text(json!([target_a_id]).to_string()),
                    SqlValue::Text("supersedes".into()),
                    SqlValue::Integer(1),
                ],
                label: Some("supersedes-actual-lookup-plan".into()),
            })
            .await
            .expect("plan the executed production statement");
        assert!(
            plan.iter().any(|row| matches!(row.get("detail"), Some(SqlValue::Text(detail))
                if detail.starts_with("SEARCH ")
                    && detail.contains("idx_graph_edges_ns_tgt_rel")
                    && detail.contains("target_id=?"))),
            "SUPERSEDES_TARGET_LOOKUP: actual recall SQL must seek the target/relation index: {plan:?}"
        );
    }
}

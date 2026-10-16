/// Create a semantic memory note whose content is `text` and return its id.
async fn seed_memory_note(rt: &KhiveRuntime, token: &NamespaceToken, text: &str) -> Uuid {
    rt.create_note(
        token,
        "memory",
        None,
        text,
        Some(0.8),
        Some(json!({ "memory_type": "semantic" })),
        vec![],
    )
    .await
    .expect("create memory note")
    .id
}

/// Batched supersedes suppression must give the answer the per-candidate
/// lookup gave, for every shape of incoming `supersedes` edge: a live edge
/// suppresses its target; a soft-deleted edge and an edge stored under another
/// namespace do not; an edge whose superseding note was soft-deleted still
/// does, because a soft delete leaves edges in place and neither lookup reads
/// the notes table. The per-candidate `query_edges` read is replayed here as the
/// oracle, so the expectation is checked against the old path and not only
/// against this test's own reading of it.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_recall_supersedes_batch_matches_per_candidate_lookup() {
    let rt = make_runtime();
    let token = rt
        .authorize(Namespace::parse("local").expect("local namespace"))
        .expect("authorize local");
    let query = "issue thirty eight eighty one supersedes batch equivalence";

    let live = seed_memory_note(&rt, &token, &format!("{query} n01")).await;
    let newer = seed_memory_note(&rt, &token, &format!("{query} n02")).await;
    let superseded = seed_memory_note(&rt, &token, &format!("{query} n03")).await;
    let edge_deleted = seed_memory_note(&rt, &token, &format!("{query} n04")).await;
    let source_deleted = seed_memory_note(&rt, &token, &format!("{query} n05")).await;
    let other_namespace = seed_memory_note(&rt, &token, &format!("{query} n06")).await;
    let removed_source = seed_memory_note(&rt, &token, &format!("{query} n07")).await;

    // A live edge: `superseded` is suppressed.
    rt.link(
        &token,
        newer,
        superseded,
        EdgeRelation::Supersedes,
        1.0,
        None,
    )
    .await
    .expect("live supersedes edge");

    // A soft-deleted edge: `edge_deleted` is not suppressed.
    let dead_edge = rt
        .link(
            &token,
            newer,
            edge_deleted,
            EdgeRelation::Supersedes,
            1.0,
            None,
        )
        .await
        .expect("supersedes edge to soft-delete");
    assert!(
        rt.delete_edge(&token, Uuid::from(dead_edge.id), false)
            .await
            .expect("soft-delete edge"),
        "the edge must exist to be soft-deleted"
    );

    // A live edge whose superseding note is then soft-deleted: `source_deleted`
    // is still suppressed.
    rt.link(
        &token,
        removed_source,
        source_deleted,
        EdgeRelation::Supersedes,
        1.0,
        None,
    )
    .await
    .expect("edge from the note about to be deleted");
    assert!(
        rt.delete_note(&token, removed_source, false)
            .await
            .expect("soft-delete superseding note"),
        "the superseding note must exist to be soft-deleted"
    );

    // A live edge under another namespace: `other_namespace` is not suppressed.
    let mut writer = rt.sql().writer().await.expect("sql writer");
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO graph_edges \
                  (namespace, id, source_id, target_id, relation, weight, created_at, updated_at) \
                  VALUES ('other', ?1, ?2, ?3, 'supersedes', 1.0, 1, 1)"
                .into(),
            params: vec![
                SqlValue::Text(Uuid::new_v4().to_string()),
                SqlValue::Text(Uuid::new_v4().to_string()),
                SqlValue::Text(other_namespace.to_string()),
            ],
            label: Some("test-3881-seed-other-namespace-supersedes".into()),
        })
        .await
        .expect("seed an edge under another namespace");
    drop(writer);

    let candidates = [
        live,
        newer,
        superseded,
        edge_deleted,
        source_deleted,
        other_namespace,
    ];
    let graph = rt.graph(&token).expect("graph store");
    let mut oracle_superseded = Vec::new();
    for id in candidates {
        let page = graph
            .query_edges(
                EdgeFilter {
                    target_ids: vec![id],
                    relations: vec![EdgeRelation::Supersedes],
                    ..EdgeFilter::default()
                },
                vec![],
                PageRequest {
                    offset: 0,
                    limit: 1,
                },
            )
            .await
            .expect("per-candidate lookup");
        if !page.items.is_empty() {
            oracle_superseded.push(id);
        }
    }
    oracle_superseded.sort();
    let mut expected_superseded = vec![superseded, source_deleted];
    expected_superseded.sort();
    assert_eq!(
        oracle_superseded, expected_superseded,
        "the per-candidate lookup must suppress exactly the live-edge targets"
    );

    let ids_of = |hits: &Value| -> Vec<Uuid> {
        let mut ids: Vec<Uuid> = hits
            .as_array()
            .expect("recall returns an array")
            .iter()
            .filter_map(|hit| hit["id"].as_str())
            .map(|id| Uuid::parse_str(id).expect("hit id is a uuid"))
            .collect();
        ids.sort();
        ids
    };
    let recall_params = |suppress: bool| {
        json!({
            "query": query,
            "limit": 20,
            "fusion_strategy": "keyword_only",
            "config": {
                "scoring": {
                    "enable_supersedes_suppression": suppress,
                    "mmr_penalty": 0.0
                }
            }
        })
    };
    let registry = make_registry(rt.clone());

    let baseline = registry
        .dispatch("memory.recall", recall_params(false))
        .await
        .expect("baseline recall");
    let mut expected_candidates = candidates.to_vec();
    expected_candidates.sort();
    assert_eq!(
        ids_of(&baseline),
        expected_candidates,
        "every seeded live note must be a candidate"
    );

    let suppressed = registry
        .dispatch("memory.recall", recall_params(true))
        .await
        .expect("suppressed recall");
    let mut expected_returned = vec![live, newer, edge_deleted, other_namespace];
    expected_returned.sort();
    assert_eq!(
        ids_of(&suppressed),
        expected_returned,
        "recall must drop exactly the candidates the per-candidate lookup flagged"
    );
}

/// Supersedes suppression asks for every candidate's incoming edge in one
/// batched statement, not one statement per candidate (#3881).
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_recall_supersedes_lookup_is_one_batched_statement() {
    const CANDIDATES: usize = 5;
    // The batched read takes this many source ids per statement.
    const BATCH_CHUNK: usize = 880;

    let rt = make_runtime();
    let token = rt
        .authorize(Namespace::parse("local").expect("local namespace"))
        .expect("authorize local");
    let query = "issue thirty eight eighty one batched supersedes statement count";
    for label in 0..CANDIDATES {
        seed_memory_note(&rt, &token, &format!("{query} n{label:02}")).await;
    }

    let registry = make_registry(rt.clone());
    let observation = rt
        .core()
        .backend()
        .pool()
        .observe_test_statement_starts(1024)
        .expect("observe recall statements");
    let hits = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": query,
                "limit": 20,
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
        .expect("recall");
    let statements = observation
        .started_statements()
        .expect("complete statement observation");
    drop(observation);

    assert_eq!(
        hits.as_array().map(Vec::len),
        Some(CANDIDATES),
        "every seeded note must be a candidate, or the statement count proves nothing"
    );
    let batched = statements
        .iter()
        .filter(|statement| {
            statement
                .sql
                .contains("edges.target_id = requested.origin_id")
                && statement.sql.contains("edges.relation IN (")
        })
        .count();
    assert_eq!(
        batched,
        CANDIDATES.div_ceil(BATCH_CHUNK),
        "the candidates' supersedes edges must be read in ceil(N / 880) statements"
    );
    let per_candidate = statements
        .iter()
        .filter(|statement| {
            statement.sql.contains("target_id IN (") && statement.sql.contains("relation IN (")
        })
        .count();
    assert_eq!(
        per_candidate, 0,
        "no per-candidate edge lookup may remain on the recall path"
    );
}

// =============================================================================
// #711: stats() must aggregate over the caller's visible namespaces, matching
// the scope a full `list` keyset walk computes over — not just token.namespace().
// =============================================================================

/// Batched stats counts must match both a full multi-namespace `list` walk and
/// the sum of the corresponding per-namespace counts, excluding soft-deleted
/// rows from every path.
#[tokio::test]
async fn stats_totals_match_list_walk_across_visible_namespaces() {
    use khive_runtime::EdgeListFilter;

    // Identity-bearing caller: actor_id configured (not anonymous).
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        packs: vec!["kg".to_string()],
        brain_profile: None,
        actor_id: Some("lambda:stats-711-test".to_string()),
        ..RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime with actor identity");

    let ns_a = Namespace::parse("stats-711-a").unwrap();
    let ns_b = Namespace::parse("stats-711-b").unwrap();
    let tok_a = rt.authorize(ns_a.clone()).unwrap();
    let tok_b = rt.authorize(ns_b.clone()).unwrap();

    // Entities: 2 in ns_a, 2 in ns_b (b2 created below, alongside the edges).
    let a1 = rt
        .create_entity_with_embedding_report(&tok_a, "concept", None, "StatsA1", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let a2 = rt
        .create_entity_with_embedding_report(&tok_a, "concept", None, "StatsA2", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    let b1 = rt
        .create_entity_with_embedding_report(&tok_b, "concept", None, "StatsB1", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();

    // Edges: two `extends` in ns_a, one `enables` in ns_b.
    rt.link(&tok_a, a1.id, a2.id, EdgeRelation::Extends, 1.0, None)
        .await
        .unwrap();
    rt.link(&tok_a, a2.id, a1.id, EdgeRelation::Extends, 1.0, None)
        .await
        .unwrap();
    let b2 = rt
        .create_entity_with_embedding_report(&tok_b, "concept", None, "StatsB2", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .unwrap();
    rt.link(&tok_b, b1.id, b2.id, EdgeRelation::Enables, 1.0, None)
        .await
        .unwrap();
    let deleted_edge = rt
        .link(&tok_b, b2.id, b1.id, EdgeRelation::Extends, 1.0, None)
        .await
        .unwrap();
    assert!(rt
        .delete_edge(&tok_b, deleted_edge.id.into(), false)
        .await
        .unwrap());

    // Notes: one in each namespace.
    rt.create_note(&tok_a, "observation", None, "NoteInA", None, None, vec![])
        .await
        .unwrap();
    rt.create_note(&tok_b, "observation", None, "NoteInB", None, None, vec![])
        .await
        .unwrap();
    let deleted_note = rt
        .create_note(
            &tok_a,
            "observation",
            None,
            "DeletedNoteInA",
            None,
            None,
            vec![],
        )
        .await
        .unwrap();
    assert!(rt
        .delete_note(&tok_a, deleted_note.id, false)
        .await
        .unwrap());

    let per_namespace_edges = rt
        .count_edges(&tok_a, EdgeListFilter::default())
        .await
        .unwrap()
        + rt.count_edges(&tok_b, EdgeListFilter::default())
            .await
            .unwrap();
    let mut per_namespace_relations = rt.count_edges_by_relation(&tok_a).await.unwrap();
    for (relation, count) in rt.count_edges_by_relation(&tok_b).await.unwrap() {
        *per_namespace_relations.entry(relation).or_insert(0) += count;
    }
    let per_namespace_notes =
        rt.count_notes(&tok_a, None).await.unwrap() + rt.count_notes(&tok_b, None).await.unwrap();

    // Identity-bearing frame whose visible set spans both namespaces.
    let vis_tok = rt
        .authorize_with_visibility(ns_a.clone(), vec![ns_b.clone()])
        .unwrap();

    let full_entities = rt
        .list_entities(&vis_tok, None, None, 500, 0)
        .await
        .unwrap();
    let stats_entities = rt.count_entities(&vis_tok, None).await.unwrap();
    assert_eq!(
        stats_entities,
        full_entities.len() as u64,
        "stats() entity total must equal a full list keyset walk under the same identity"
    );
    assert_eq!(stats_entities, 4);

    let full_edges = rt
        .list_edges(&vis_tok, EdgeListFilter::default(), 1000, 0)
        .await
        .unwrap();
    let stats_edges = rt
        .count_edges(&vis_tok, EdgeListFilter::default())
        .await
        .unwrap();
    assert_eq!(
        stats_edges,
        full_edges.len() as u64,
        "stats() edge total must equal a full list keyset walk under the same identity"
    );
    assert_eq!(stats_edges, per_namespace_edges);
    assert_eq!(stats_edges, 3);

    let edges_by_relation = rt.count_edges_by_relation(&vis_tok).await.unwrap();
    assert_eq!(edges_by_relation, per_namespace_relations);
    let relation_sum: u64 = edges_by_relation.values().sum();
    assert_eq!(
        relation_sum, stats_edges,
        "edges_by_relation must sum to the edges scalar under all identities"
    );

    // `stats()` reads the three edge counts as one grouped aggregate, which
    // must answer exactly what the separate counts answer.
    let combined = rt.edge_stats_counts(&vis_tok).await.unwrap();
    assert_eq!(combined.edges, stats_edges);
    assert_eq!(combined.edges_by_relation, edges_by_relation);
    assert_eq!(
        combined.edges_by_endpoint_base,
        rt.count_edges_by_endpoint_base(&vis_tok).await.unwrap()
    );

    let full_notes = rt.list_notes(&vis_tok, None, 200, 0).await.unwrap();
    let stats_notes = rt.count_notes(&vis_tok, None).await.unwrap();
    assert_eq!(
        stats_notes,
        full_notes.len() as u64,
        "stats() note total must equal a full list keyset walk under the same identity"
    );
    assert_eq!(stats_notes, per_namespace_notes);
    assert_eq!(stats_notes, 2);
}

#[test]
#[serial_test::serial]
fn test_harness_refuses_runtime_default_store() {
    if test_process::run_in_child() {
        return;
    }

    struct HomeGuard(Option<std::ffi::OsString>);

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(home) => std::env::set_var("HOME", home),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    assert_eq!(
        std::env::var("KHIVE_TEST_HARNESS").as_deref(),
        Ok("1"),
        "the workspace Cargo harness must mark integration-test processes"
    );
    let fake_home = tempfile::tempdir().expect("temporary HOME");
    let _home_guard = HomeGuard(std::env::var_os("HOME"));
    std::env::set_var("HOME", fake_home.path());
    let expected_path = fake_home.path().join(".khive/khive.db");
    let config = RuntimeConfig::no_embeddings();
    assert_eq!(config.db_path.as_deref(), Some(expected_path.as_path()));

    let error = match KhiveRuntime::new(config) {
        Ok(_) => panic!("test harness opened the default home-directory store"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("test harness refused"),
        "unexpected guard error: {error}"
    );
    assert!(
        !expected_path.exists(),
        "the guarded runtime must refuse the path before SQLite creates it"
    );
}

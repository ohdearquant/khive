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

/// Default recall suppresses a local candidate using an incoming edge stored
/// in another namespace that is already visible to the caller.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn default_recall_suppresses_edges_in_every_visible_namespace() {
    let visible = Namespace::parse("visible").expect("visible namespace");
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: vec![],
        visible_namespaces: vec![visible.clone()],
        ..RuntimeConfig::default()
    })
    .expect("in-memory runtime");
    let local_token = rt
        .authorize(Namespace::local())
        .expect("authorize local");
    let visible_token = rt
        .authorize(visible.clone())
        .expect("authorize visible namespace");
    let query = "visible namespace edge suppresses old memory";
    let old = rt
        .create_note(
            &local_token,
            "memory",
            None,
            query,
            Some(0.8),
            Some(json!({"memory_type": "semantic"})),
            vec![],
        )
        .await
        .expect("create old local memory");
    let newer = rt
        .create_note(
            &visible_token,
            "memory",
            None,
            "unrelated replacement text",
            Some(0.8),
            Some(json!({"memory_type": "semantic"})),
            vec![],
        )
        .await
        .expect("create non-matching replacement memory");
    rt.link(
        &visible_token,
        newer.id,
        old.id,
        EdgeRelation::Supersedes,
        1.0,
        None,
    )
    .await
    .expect("store supersedes edge in visible namespace");

    let mut builder = VerbRegistryBuilder::new();
    builder.with_visible_namespaces(vec![visible]);
    builder.register(KgPack::new(rt.clone()));
    builder.register(MemoryPack::new(
        receipt_credentials::with_receipt_credentials(rt),
    ));
    let registry = builder.build().expect("registry builds");
    let baseline = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": query,
                "limit": 10,
                "fusion_strategy": "keyword_only",
                "config": {"scoring": {
                    "enable_supersedes_suppression": false,
                    "mmr_penalty": 0.0
                }}
            }),
        )
        .await
        .expect("baseline recall succeeds");
    let baseline_ids: Vec<&str> = baseline
        .as_array()
        .expect("baseline recall returns an array")
        .iter()
        .filter_map(|hit| hit["id"].as_str())
        .collect();
    let old_id = old.id.to_string();
    let newer_id = newer.id.to_string();
    assert!(baseline_ids.contains(&old_id.as_str()));
    assert!(!baseline_ids.contains(&newer_id.as_str()));
    let hits = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": query,
                "limit": 10,
                "fusion_strategy": "keyword_only",
                "config": {"scoring": {"mmr_penalty": 0.0}}
            }),
        )
        .await
        .expect("default recall succeeds");
    assert!(
        hits.as_array()
            .expect("recall returns an array")
            .iter()
            .all(|hit| hit["id"].as_str() != Some(old_id.as_str())),
        "default recall must suppress the old memory by its visible incoming edge: {hits}"
    );
}

/// An explicit namespace narrows the edge read to that namespace: an old row in
/// `visible` whose `supersedes` edge sits in `local` is returned to a request
/// for `visible`, and hidden from a default request, which sees both.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn explicit_namespace_recall_reads_edges_only_where_it_can_see() {
    let visible = Namespace::parse("visible").expect("visible namespace");
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: vec![],
        visible_namespaces: vec![visible.clone()],
        ..RuntimeConfig::default()
    })
    .expect("in-memory runtime");
    let local_token = rt.authorize(Namespace::local()).expect("authorize local");
    let visible_token = rt
        .authorize(visible.clone())
        .expect("authorize visible namespace");
    let query = "explicit namespace narrows edge reads";
    let old = rt
        .create_note(
            &visible_token,
            "memory",
            None,
            query,
            Some(0.8),
            Some(json!({"memory_type": "semantic"})),
            vec![],
        )
        .await
        .expect("create old memory in the visible namespace");
    let newer = rt
        .create_note(
            &local_token,
            "memory",
            None,
            "unrelated replacement text",
            Some(0.8),
            Some(json!({"memory_type": "semantic"})),
            vec![],
        )
        .await
        .expect("create non-matching replacement memory");
    rt.link(
        &local_token,
        newer.id,
        old.id,
        EdgeRelation::Supersedes,
        1.0,
        None,
    )
    .await
    .expect("store supersedes edge in local");

    let mut builder = VerbRegistryBuilder::new();
    builder.with_visible_namespaces(vec![visible.clone()]);
    builder.register(KgPack::new(rt.clone()));
    builder.register(MemoryPack::new(
        receipt_credentials::with_receipt_credentials(rt),
    ));
    let registry = builder.build().expect("registry builds");
    let ids = |hits: &serde_json::Value| -> Vec<String> {
        hits.as_array()
            .expect("recall returns an array")
            .iter()
            .filter_map(|hit| hit["id"].as_str().map(str::to_owned))
            .collect()
    };
    let old_id = old.id.to_string();

    let explicit = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": query,
                "namespace": visible.as_str(),
                "limit": 10,
                "fusion_strategy": "keyword_only",
                "config": {"scoring": {"mmr_penalty": 0.0}}
            }),
        )
        .await
        .expect("explicit namespace recall succeeds");
    assert!(
        ids(&explicit).contains(&old_id),
        "an edge outside the explicit namespace must not hide the old memory: {explicit}"
    );

    let default = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": query,
                "limit": 10,
                "fusion_strategy": "keyword_only",
                "config": {"scoring": {"mmr_penalty": 0.0}}
            }),
        )
        .await
        .expect("default recall succeeds");
    assert!(
        !ids(&default).contains(&old_id),
        "a default recall sees local and must hide the old memory: {default}"
    );
}

/// Property suppression accepts full UUIDs and long lowercase hex prefixes,
/// while empty and one-character values suppress no ranked candidate. These
/// legacy rows are written through NoteStore::upsert_note, below memory.remember.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn recall_property_suppression_requires_uuid_or_long_hex_prefix() {
    let rt = make_runtime();
    let token = rt
        .authorize(Namespace::local())
        .expect("authorize local");
    // Targets start with a0, a1, a2, a3 and sources with b0..b3, so a value
    // derived from one case's target can name no other row.
    let words = ["alpha", "bravo", "charlie", "delta"];
    let mut expected = Vec::new();
    for (index, word) in words.into_iter().enumerate() {
        let query = format!("property suppression {word} marker");
        let target_id = Uuid::from_u128(((0xa0 + index as u128) << 120) | 1);
        let source_id = Uuid::from_u128(((0xb0 + index as u128) << 120) | 2);
        let target_text = target_id.to_string();
        let supersedes = match index {
            0 => String::new(),
            1 => target_text[..1].to_owned(),
            2 => target_text.clone(),
            _ => target_text[..8].to_owned(),
        };
        let mut target = khive_storage::Note::new("local", "memory", &query)
            .with_salience(0.8)
            .with_properties(json!({"memory_type": "semantic"}));
        target.id = target_id;
        let mut source = khive_storage::Note::new("local", "memory", &query)
            .with_salience(0.8)
            .with_properties(json!({
                "memory_type": "semantic",
                "supersedes": supersedes.as_str()
            }));
        source.id = source_id;
        let notes = rt.notes(&token).expect("note store");
        notes.upsert_note(target).await.expect("store target note");
        notes
            .upsert_note(source)
            .await
            .expect("store source note with legacy property");
        // upsert_note writes the note row only. Keyword recall reads the text
        // index, so each row gets the text document memory.remember would write.
        let text = rt.text_for_notes(&token).expect("text store");
        for subject_id in [target_id, source_id] {
            text.upsert_document(khive_storage::TextDocument {
                subject_id,
                kind: khive_types::SubstrateKind::Note,
                record_kind: Some("memory".into()),
                namespace: "local".into(),
                title: None,
                body: query.clone(),
                tags: vec![],
                metadata: None,
                updated_at: chrono::Utc::now(),
            })
            .await
            .expect("index note text");
        }
        expected.push((query, supersedes, target_id, source_id));
    }

    let registry = make_registry(rt);
    for (query, supersedes, target_id, source_id) in expected {
        let hits = registry
            .dispatch(
                "memory.recall",
                json!({
                    "query": query,
                    "limit": 10,
                    "fusion_strategy": "keyword_only",
                    "config": {"scoring": {"mmr_penalty": 0.0}}
                }),
            )
            .await
            .expect("recall succeeds");
        let ids: Vec<Uuid> = hits
            .as_array()
            .expect("recall returns an array")
            .iter()
            .filter_map(|hit| hit["id"].as_str())
            .map(|id| Uuid::parse_str(id).expect("hit ID is a UUID"))
            .collect();
        if supersedes.len() < 8 {
            assert!(
                ids.contains(&target_id),
                "invalid property {supersedes:?} hid target"
            );
            assert!(
                ids.contains(&source_id),
                "invalid property {supersedes:?} hid source"
            );
        } else {
            assert!(
                !ids.contains(&target_id),
                "valid property {supersedes:?} missed target"
            );
            assert!(
                ids.contains(&source_id),
                "valid property {supersedes:?} hid source"
            );
        }
    }
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

use super::*;
use crate::migrations::run_migrations;
use crate::pool::PoolConfig;
use std::time::Duration;
use tokio::sync::oneshot;

fn setup_pool() -> Arc<ConnectionPool> {
    let config = PoolConfig {
        path: None,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(ENTITIES_DDL).unwrap();
    }
    pool
}

fn setup_memory_store() -> SqlEntityStore {
    SqlEntityStore::new(setup_pool(), false)
}

fn setup_memory_store_ns(_ns: &str) -> SqlEntityStore {
    SqlEntityStore::new(setup_pool(), false)
}

fn make_entity(namespace: &str, kind: &str, name: &str) -> Entity {
    let now = chrono::Utc::now().timestamp_micros();
    Entity {
        id: Uuid::new_v4(),
        namespace: namespace.to_string(),
        kind: kind.to_string(),
        entity_type: None,
        name: name.to_string(),
        description: None,
        properties: None,
        tags: Vec::new(),
        created_at: now,
        updated_at: now,
        deleted_at: None,
        merged_into: None,
        merge_event_id: None,
    }
}

#[test]
fn case_insensitive_candidate_lookup_uses_one_partial_index_seek_per_candidate() {
    let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    run_migrations(&mut conn).unwrap();

    let filter = EntityFilter {
        names_ci: vec!["lora".to_string(), "北京大学".to_string()],
        ..EntityFilter::default()
    };
    let mut lookup_filter = filter.clone();
    lookup_filter.names_ci.clear();
    let (where_sql, mut params) = build_entity_where("local", &lookup_filter);
    let candidate_param_indices: Vec<usize> = filter
        .names_ci
        .iter()
        .map(|candidate| {
            params.push(Box::new(candidate.to_ascii_lowercase()));
            params.len()
        })
        .collect();
    params.push(Box::new(64_i64));
    params.push(Box::new(0_i64));
    let limit_idx = params.len() - 1;
    let offset_idx = params.len();
    let data_sql = build_candidate_entity_query(
        "id",
        &where_sql,
        &candidate_param_indices,
        "created_at DESC",
        limit_idx,
        offset_idx,
    );
    assert!(!data_sql.contains("GROUP BY"));
    assert!(data_sql.contains("FROM candidates"));
    assert!(data_sql.contains("LIMIT 1"));

    let sql = format!("EXPLAIN QUERY PLAN {data_sql}");
    let mut stmt = conn.prepare(&sql).unwrap();
    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        params.iter().map(|param| param.as_ref()).collect();
    let details: Vec<String> = stmt
        .query_map(param_refs.as_slice(), |row| row.get(3))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();

    assert!(
        details.iter().any(|detail| {
            detail.contains("SEARCH entities USING INDEX idx_entities_namespace_name_ci")
        }),
        "candidate lookup must seek the case-insensitive expression index: {details:?}"
    );
    assert!(
        details
            .iter()
            .any(|detail| detail.contains("CORRELATED SCALAR SUBQUERY")),
        "candidate relation must drive one scalar lookup per name: {details:?}"
    );
    assert!(
        !details
            .iter()
            .any(|detail| detail.contains("SCAN entities")),
        "candidate lookup must not scan entities: {details:?}"
    );

    let index_sql: String = conn
        .query_row(
            "SELECT sql FROM sqlite_master \
             WHERE type = 'index' AND name = 'idx_entities_namespace_name_ci'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        index_sql.contains("WHERE deleted_at IS NULL"),
        "candidate lookup index must exclude tombstones: {index_sql}"
    );
}

#[tokio::test]
async fn case_insensitive_candidate_lookup_skips_unbounded_total_count() {
    let store = setup_memory_store();
    store
        .upsert_entity(make_entity("local", "concept", "LoRA"))
        .await
        .unwrap();

    let page = store
        .query_entities(
            "local",
            EntityFilter {
                names_ci: vec!["lora".to_string()],
                ..EntityFilter::default()
            },
            PageRequest {
                limit: 64,
                offset: 0,
            },
        )
        .await
        .unwrap();

    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].name, "LoRA");
    assert_eq!(page.total, None);
}

#[tokio::test]
async fn case_insensitive_candidate_lookup_caps_folded_names_not_duplicate_rows() {
    let store = setup_memory_store();
    let mut older_b = make_entity("local", "concept", "crowdbeta");
    older_b.created_at = 1;
    older_b.updated_at = 1;
    store.upsert_entity(older_b).await.unwrap();

    for created_at in 2..=258 {
        let mut newer_a = make_entity("local", "concept", "CrowdAlpha");
        newer_a.created_at = created_at;
        newer_a.updated_at = created_at;
        store.upsert_entity(newer_a).await.unwrap();
    }

    let page = store
        .query_entities(
            "local",
            EntityFilter {
                names_ci: vec![
                    "CrowdAlpha".to_string(),
                    "crowdalpha".to_string(),
                    "crowdbeta".to_string(),
                ],
                ..EntityFilter::default()
            },
            PageRequest {
                limit: 256,
                offset: 0,
            },
        )
        .await
        .unwrap();

    let folded_names: Vec<String> = page
        .items
        .iter()
        .map(|entity| entity.name.to_lowercase())
        .collect();
    assert_eq!(folded_names.len(), 2);
    assert!(folded_names.contains(&"crowdalpha".to_string()));
    assert!(folded_names.contains(&"crowdbeta".to_string()));
}

#[tokio::test]
async fn test_upsert_and_get_entity() {
    let store = setup_memory_store();

    let entity = make_entity("default", "concept", "LoRA");
    let id = entity.id;

    store.upsert_entity(entity).await.unwrap();

    let fetched = store.get_entity(id).await.unwrap();
    assert!(fetched.is_some());
    let fetched = fetched.unwrap();
    assert_eq!(fetched.id, id);
    assert_eq!(fetched.name, "LoRA");
    assert_eq!(fetched.kind, "concept");
}

#[tokio::test]
async fn test_upsert_with_builder() {
    let store = setup_memory_store();

    let props = serde_json::json!({"domain": "fine-tuning", "type": "technique"});
    let entity = Entity::new("default", "concept", "QLoRA")
        .with_description("Quantized LoRA")
        .with_properties(props.clone())
        .with_tags(vec!["fine-tuning".to_string(), "quantization".to_string()]);
    let id = entity.id;

    store.upsert_entity(entity).await.unwrap();

    let fetched = store.get_entity(id).await.unwrap().unwrap();
    assert_eq!(fetched.description.as_deref(), Some("Quantized LoRA"));
    assert_eq!(fetched.properties, Some(props));
    assert_eq!(fetched.tags, vec!["fine-tuning", "quantization"]);
}

#[tokio::test]
async fn test_soft_delete() {
    let store = setup_memory_store();

    let entity = make_entity("default", "concept", "to-delete");
    let id = entity.id;
    store.upsert_entity(entity).await.unwrap();

    let deleted = store.delete_entity(id, DeleteMode::Soft).await.unwrap();
    assert!(deleted);

    let fetched = store.get_entity(id).await.unwrap();
    assert!(fetched.is_none());
}

#[tokio::test]
async fn test_hard_delete() {
    let store = setup_memory_store();

    let entity = make_entity("default", "concept", "to-hard-delete");
    let id = entity.id;
    store.upsert_entity(entity).await.unwrap();

    let deleted = store.delete_entity(id, DeleteMode::Hard).await.unwrap();
    assert!(deleted);

    let fetched = store.get_entity(id).await.unwrap();
    assert!(fetched.is_none());
}

#[tokio::test]
async fn test_query_entities_basic() {
    let store = setup_memory_store_ns("ns1");

    for name in &["Alpha", "Beta", "Gamma"] {
        store
            .upsert_entity(make_entity("ns1", "concept", name))
            .await
            .unwrap();
    }
    store
        .upsert_entity(make_entity("ns1", "document", "Paper1"))
        .await
        .unwrap();

    let page = store
        .query_entities(
            "ns1",
            EntityFilter::default(),
            PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 4);
    assert_eq!(page.total, Some(4));

    // Filter by kind
    let concepts = store
        .query_entities(
            "ns1",
            EntityFilter {
                kinds: vec!["concept".to_string()],
                ..Default::default()
            },
            PageRequest::default(),
        )
        .await
        .unwrap();
    assert_eq!(concepts.items.len(), 3);
}

#[tokio::test]
async fn test_query_by_name_prefix() {
    let store = setup_memory_store_ns("ns1");

    // "Alpha" and "AlphaGo" both start with "Alpha"; "Beta" does not
    for &name in &["Alpha", "AlphaGo", "Beta"] {
        store
            .upsert_entity(make_entity("ns1", "concept", name))
            .await
            .unwrap();
    }

    let result = store
        .query_entities(
            "ns1",
            EntityFilter {
                name_prefix: Some("Alpha".to_string()),
                ..Default::default()
            },
            PageRequest::default(),
        )
        .await
        .unwrap();
    assert_eq!(result.items.len(), 2);
    let names: Vec<&str> = result.items.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"Alpha"), "Alpha not found in {names:?}");
    assert!(names.contains(&"AlphaGo"), "AlphaGo not found in {names:?}");
    assert!(!names.contains(&"Beta"));
}

#[tokio::test]
async fn test_query_by_name_prefix_escapes_underscore_wildcard() {
    let store = setup_memory_store_ns("ns1");

    // The real entity's name contains a literal underscore.
    store
        .upsert_entity(make_entity("ns1", "concept", "a_b"))
        .await
        .unwrap();

    // 150 decoys that would match the *unescaped* LIKE pattern "a_b%"
    // (`_` as a wildcard matches any single character) but must not match
    // once the wildcard is escaped. Created after "a_b" so a page ordered
    // by created_at DESC with LIMIT 100 would push "a_b" out unless exact
    // matches are also ranked first.
    for i in 0..150 {
        store
            .upsert_entity(make_entity("ns1", "concept", &format!("aXb-{i:03}")))
            .await
            .unwrap();
    }

    let result = store
        .query_entities(
            "ns1",
            EntityFilter {
                name_prefix: Some("a_b".to_string()),
                ..Default::default()
            },
            PageRequest {
                offset: 0,
                limit: 100,
            },
        )
        .await
        .unwrap();

    let names: Vec<&str> = result.items.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"a_b"),
        "exact match 'a_b' must survive escaping and page ordering despite 150 wildcard-matching decoys; got {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.starts_with("aXb")),
        "escaped '_' must not match decoy names like 'aXb-000'; got {names:?}"
    );
}

#[tokio::test]
async fn test_query_by_name_prefix_escapes_percent_wildcard() {
    let store = setup_memory_store_ns("ns1");

    // The real entity's name contains a literal percent sign.
    store
        .upsert_entity(make_entity("ns1", "concept", "50%off"))
        .await
        .unwrap();

    // 150 decoys that would match the *unescaped* LIKE pattern "50%off%"
    // (`%` as a wildcard matches any sequence, including across "-") but
    // must not match once the wildcard is escaped.
    for i in 0..150 {
        store
            .upsert_entity(make_entity("ns1", "concept", &format!("50-off-{i:03}")))
            .await
            .unwrap();
    }

    let result = store
        .query_entities(
            "ns1",
            EntityFilter {
                name_prefix: Some("50%off".to_string()),
                ..Default::default()
            },
            PageRequest {
                offset: 0,
                limit: 100,
            },
        )
        .await
        .unwrap();

    let names: Vec<&str> = result.items.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"50%off"),
        "exact match '50%off' must survive escaping and page ordering despite 150 wildcard-matching decoys; got {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.starts_with("50-off-")),
        "escaped '%' must not match decoy names like '50-off-000'; got {names:?}"
    );
}

// Regression for a round-1 gap on #818/#834: the two tests above prove
// escaping keeps decoys out of the WHERE clause entirely, so they pass even
// without the exact-match-first ORDER BY CASE. This test isolates that
// ordering: every decoy genuinely matches the LIKE pattern "Base%" (no
// wildcard characters, escaping plays no role) and is newer than the exact
// match, so a page ordered only by `created_at DESC` would fill entirely with
// decoys and strand "Base" past the LIMIT 100 boundary. It fails if the
// `ORDER BY CASE WHEN LOWER(name) = ... THEN 0 ELSE 1 END` clause is removed.
#[tokio::test]
async fn test_query_by_name_prefix_exact_match_ranked_before_many_matching_decoys() {
    let store = setup_memory_store_ns("ns1");

    store
        .upsert_entity(make_entity("ns1", "concept", "Base"))
        .await
        .unwrap();

    for i in 0..150 {
        store
            .upsert_entity(make_entity("ns1", "concept", &format!("Base-{i:03}")))
            .await
            .unwrap();
    }

    let result = store
        .query_entities(
            "ns1",
            EntityFilter {
                name_prefix: Some("Base".to_string()),
                ..Default::default()
            },
            PageRequest {
                offset: 0,
                limit: 100,
            },
        )
        .await
        .unwrap();

    let names: Vec<&str> = result.items.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"Base"),
        "exact match 'Base' must be ranked ahead of 150 newer, genuinely prefix-matching \
         decoys within a LIMIT 100 page; got {names:?}"
    );
}

#[tokio::test]
async fn test_count_entities() {
    let store = setup_memory_store_ns("ns1");

    for _ in 0..5 {
        store
            .upsert_entity(make_entity("ns1", "concept", "X"))
            .await
            .unwrap();
    }

    let count = store
        .count_entities("ns1", EntityFilter::default())
        .await
        .unwrap();
    assert_eq!(count, 5);

    // Namespace is the caller's responsibility — querying "ns2" returns 0
    // because no entities were inserted in that namespace.
    let count_other = store
        .count_entities("ns2", EntityFilter::default())
        .await
        .unwrap();
    assert_eq!(count_other, 0);
}

#[tokio::test]
async fn test_batch_upsert() {
    let store = setup_memory_store_ns("batch_ns");

    let entities: Vec<Entity> = (0..10)
        .map(|i| make_entity("batch_ns", "concept", &format!("entity_{i}")))
        .collect();

    let summary = store.upsert_entities(entities).await.unwrap();
    assert_eq!(summary.attempted, 10);
    assert_eq!(summary.affected, 10);
    assert_eq!(summary.failed, 0);

    let count = store
        .count_entities("batch_ns", EntityFilter::default())
        .await
        .unwrap();
    assert_eq!(count, 10);
}

/// One store, two namespaces — each query sees only its own.
#[tokio::test]
async fn test_namespace_isolation() {
    let pool = setup_pool();
    let store = SqlEntityStore::new(Arc::clone(&pool), false);

    store
        .upsert_entity(make_entity("ns_a", "concept", "EntityA"))
        .await
        .unwrap();
    store
        .upsert_entity(make_entity("ns_b", "concept", "EntityB"))
        .await
        .unwrap();

    // Namespace is the caller's responsibility — pass it in the query.
    let count_a = store
        .count_entities("ns_a", EntityFilter::default())
        .await
        .unwrap();
    let count_b = store
        .count_entities("ns_b", EntityFilter::default())
        .await
        .unwrap();

    assert_eq!(count_a, 1);
    assert_eq!(count_b, 1);

    let page_a = store
        .query_entities("ns_a", EntityFilter::default(), PageRequest::default())
        .await
        .unwrap();
    assert_eq!(page_a.items[0].name, "EntityA");

    let page_b = store
        .query_entities("ns_b", EntityFilter::default(), PageRequest::default())
        .await
        .unwrap();
    assert_eq!(page_b.items[0].name, "EntityB");
}

#[tokio::test]
async fn test_query_by_tags() {
    let store = setup_memory_store_ns("tags_ns");

    let mut e1 = make_entity("tags_ns", "concept", "Tagged1");
    e1.tags = vec!["rust".to_string(), "systems".to_string()];
    let mut e2 = make_entity("tags_ns", "concept", "Tagged2");
    e2.tags = vec!["python".to_string(), "ml".to_string()];
    let mut e3 = make_entity("tags_ns", "concept", "Tagged3");
    e3.tags = vec!["rust".to_string(), "ml".to_string()];

    store.upsert_entity(e1).await.unwrap();
    store.upsert_entity(e2).await.unwrap();
    store.upsert_entity(e3).await.unwrap();

    // Filter by "rust" tag — should match Tagged1 and Tagged3
    let result = store
        .query_entities(
            "tags_ns",
            EntityFilter {
                tags_any: vec!["rust".to_string()],
                ..Default::default()
            },
            PageRequest::default(),
        )
        .await
        .unwrap();
    assert_eq!(result.items.len(), 2);
    let names: Vec<&str> = result.items.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"Tagged1"));
    assert!(names.contains(&"Tagged3"));
    assert!(!names.contains(&"Tagged2"));

    // Filter by "ml" tag — should match Tagged2 and Tagged3
    let result = store
        .query_entities(
            "tags_ns",
            EntityFilter {
                tags_any: vec!["ml".to_string()],
                ..Default::default()
            },
            PageRequest::default(),
        )
        .await
        .unwrap();
    assert_eq!(result.items.len(), 2);

    // Filter by both "rust" and "python" (union) — should match all three
    let result = store
        .query_entities(
            "tags_ns",
            EntityFilter {
                tags_any: vec!["rust".to_string(), "python".to_string()],
                ..Default::default()
            },
            PageRequest::default(),
        )
        .await
        .unwrap();
    assert_eq!(result.items.len(), 3);
}

#[tokio::test]
async fn test_query_by_ids() {
    let store = setup_memory_store_ns("ns1");

    let e1 = make_entity("ns1", "concept", "E1");
    let e2 = make_entity("ns1", "concept", "E2");
    let e3 = make_entity("ns1", "concept", "E3");
    let ids = vec![e1.id, e3.id];

    store.upsert_entity(e1).await.unwrap();
    store.upsert_entity(e2).await.unwrap();
    store.upsert_entity(e3).await.unwrap();

    let result = store
        .query_entities(
            "ns1",
            EntityFilter {
                ids,
                ..Default::default()
            },
            PageRequest::default(),
        )
        .await
        .unwrap();
    assert_eq!(result.items.len(), 2);
    let names: Vec<&str> = result.items.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"E1"));
    assert!(names.contains(&"E3"));
    assert!(!names.contains(&"E2"));
}

#[tokio::test]
async fn test_entity_type_roundtrip() {
    let store = setup_memory_store();

    let entity =
        Entity::new("default", "document", "ResearchPaper").with_entity_type(Some("paper"));
    let id = entity.id;

    store.upsert_entity(entity).await.unwrap();

    let fetched = store.get_entity(id).await.unwrap().unwrap();
    assert_eq!(fetched.entity_type, Some("paper".to_string()));
    assert_eq!(fetched.kind, "document");
    assert_eq!(fetched.name, "ResearchPaper");
}

#[tokio::test]
async fn test_query_by_kind_and_entity_type() {
    let store = setup_memory_store_ns("et_ns");

    let typed = Entity::new("et_ns", "person", "Researcher").with_entity_type(Some("researcher"));
    let untyped = make_entity("et_ns", "person", "Generic");

    store.upsert_entity(typed).await.unwrap();
    store.upsert_entity(untyped).await.unwrap();

    let result = store
        .query_entities(
            "et_ns",
            EntityFilter {
                entity_types: vec!["researcher".to_string()],
                ..Default::default()
            },
            PageRequest::default(),
        )
        .await
        .unwrap();

    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].name, "Researcher");
    assert_eq!(result.items[0].entity_type, Some("researcher".to_string()));
}

/// UUID is globally unique (id TEXT PRIMARY KEY). Upserting the same UUID in a
/// different namespace overwrites the row (INSERT OR REPLACE). get_entity by ID
/// returns whichever namespace currently owns that UUID.
#[tokio::test]
async fn test_same_id_upsert_replaces_row() {
    let pool = setup_pool();
    let store = SqlEntityStore::new(Arc::clone(&pool), false);

    let shared_id = Uuid::new_v4();
    let now = chrono::Utc::now().timestamp_micros();

    let entity_a = Entity {
        id: shared_id,
        namespace: "ns_a".to_string(),
        kind: "concept".to_string(),
        entity_type: None,
        name: "SharedInA".to_string(),
        description: None,
        properties: None,
        tags: Vec::new(),
        created_at: now,
        updated_at: now,
        deleted_at: None,
        merged_into: None,
        merge_event_id: None,
    };
    store.upsert_entity(entity_a).await.unwrap();

    // At this point the row is in ns_a.
    let fetched = store.get_entity(shared_id).await.unwrap().unwrap();
    assert_eq!(fetched.namespace, "ns_a");
    assert_eq!(fetched.name, "SharedInA");

    // Upsert same UUID into ns_b — INSERT OR REPLACE replaces the row.
    let entity_b = Entity {
        id: shared_id,
        namespace: "ns_b".to_string(),
        kind: "concept".to_string(),
        entity_type: None,
        name: "SharedInB".to_string(),
        description: None,
        properties: None,
        tags: Vec::new(),
        created_at: now,
        updated_at: now,
        deleted_at: None,
        merged_into: None,
        merge_event_id: None,
    };
    store.upsert_entity(entity_b).await.unwrap();

    // Now the row is in ns_b — get_entity returns ns_b regardless of which namespace
    // you query from (namespace is caller's responsibility).
    let fetched = store.get_entity(shared_id).await.unwrap().unwrap();
    assert_eq!(fetched.namespace, "ns_b");
    assert_eq!(fetched.name, "SharedInB");

    // ns_a now has 0 entities; ns_b has 1.
    let count_a = store
        .count_entities("ns_a", EntityFilter::default())
        .await
        .unwrap();
    let count_b = store
        .count_entities("ns_b", EntityFilter::default())
        .await
        .unwrap();
    assert_eq!(count_a, 0);
    assert_eq!(count_b, 1);
}

/// STORAGE-AUD-003 / #485: PageRequest.offset > i64::MAX must return
/// InvalidInput instead of silently narrowing to a negative i64 offset.
#[tokio::test]
async fn page_offset_over_i64max_rejected() {
    let store = setup_memory_store_ns("ns1");
    store
        .upsert_entity(make_entity("ns1", "concept", "Alpha"))
        .await
        .unwrap();

    let result = store
        .query_entities(
            "ns1",
            EntityFilter::default(),
            PageRequest {
                offset: (i64::MAX as u64) + 1,
                limit: 10,
            },
        )
        .await;

    assert!(
        matches!(result, Err(StorageError::InvalidInput { .. })),
        "expected InvalidInput, got {result:?}"
    );
}

// =============================================================================
// ADR-067 slice 1: WriterTask-routed `upsert_entities` (KHIVE_WRITE_QUEUE=1)
// =============================================================================

/// Flag-ON mechanism: with `KHIVE_WRITE_QUEUE=1`, `upsert_entities` routes
/// through the WriterTask channel instead of the pool-mutex path, and the
/// `BatchWriteSummary` fields (attempted/affected/failed/first_error) survive
/// the trip through the type-erased channel intact, and both rows are
/// actually committed and independently readable back through the store.
///
/// Constructed via a `PoolConfig` literal (`write_queue_enabled: true`), not
/// the `KHIVE_WRITE_QUEUE` env var — that env var is process-global and this
/// crate's other tests are NOT `#[serial]` against it, so a window where it
/// is set here could leak into a concurrently-scheduled test's own pool
/// construction (ADR-067 Fork C slice 2 round 2, LOW finding).
#[tokio::test]
async fn upsert_entities_routes_through_writer_task_when_flag_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("write_queue_entities.db");
    let pool_cfg = PoolConfig {
        path: Some(path.clone()),
        write_queue_enabled: true,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(pool_cfg).unwrap());
    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(ENTITIES_DDL).unwrap();
    }

    let store = SqlEntityStore::new(Arc::clone(&pool), true);

    let e1 = make_entity("default", "concept", "LoRA");
    let e2 = make_entity("default", "concept", "QLoRA");
    let id1 = e1.id;
    let id2 = e2.id;

    let summary = store.upsert_entities(vec![e1, e2]).await.unwrap();
    assert_eq!(summary.attempted, 2);
    assert_eq!(summary.affected, 2);
    assert_eq!(summary.failed, 0);
    assert!(summary.first_error.is_empty());

    let fetched1 = store.get_entity(id1).await.unwrap();
    assert!(
        fetched1.is_some(),
        "entity 1 must be committed and readable"
    );
    assert_eq!(fetched1.unwrap().name, "LoRA");

    let fetched2 = store.get_entity(id2).await.unwrap();
    assert!(
        fetched2.is_some(),
        "entity 2 must be committed and readable"
    );
    assert_eq!(fetched2.unwrap().name, "QLoRA");
}

/// Flag-OFF regression (explicit): with the flag at its default (off), the
/// store never spawns a writer task, and `upsert_entities` still returns a
/// correct `BatchWriteSummary` via the legacy pool-mutex path — the same
/// shape `test_batch_upsert` above already covers, restated here to
/// document the flag-off/flag-on pairing for ADR-067 slice 1.
#[tokio::test]
async fn upsert_entities_legacy_path_unchanged_when_flag_is_off() {
    let store = setup_memory_store();

    let e1 = make_entity("default", "concept", "LoRA");
    let e2 = make_entity("default", "concept", "QLoRA");

    let summary = store.upsert_entities(vec![e1, e2]).await.unwrap();
    assert_eq!(summary.attempted, 2);
    assert_eq!(summary.affected, 2);
    assert_eq!(summary.failed, 0);
}

/// ADR-067 Component A's whole point is ONE writer owning the write
/// connection for a DB file — a per-store writer task would let concurrent
/// stores over the same pool spawn independent writer connections that
/// contend with each other at `BEGIN IMMEDIATE`, so the migrated path would
/// race itself instead of eliminating write contention. Constructing
/// several `SqlEntityStore`s over the SAME pool with the flag on must spawn
/// the writer task exactly once; every store resolves to a clone of the one
/// pool-owned handle (`ConnectionPool::writer_task_handle`).
///
/// Constructed via a `PoolConfig` literal (`write_queue_enabled: true`), not
/// the `KHIVE_WRITE_QUEUE` env var — see the sibling
/// `upsert_entities_routes_through_writer_task_when_flag_enabled` test's doc
/// comment for the race this avoids.
#[tokio::test]
async fn multiple_stores_over_one_pool_share_a_single_writer_task() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("write_queue_shared_writer.db");
    let pool_cfg = PoolConfig {
        path: Some(path.clone()),
        write_queue_enabled: true,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(pool_cfg).unwrap());
    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(ENTITIES_DDL).unwrap();
    }

    // Three independent stores over the same pool, each resolving the
    // write-queue flag on construction and asking the pool for its writer
    // task — none of them must trigger a second spawn.
    let _store1 = SqlEntityStore::new(Arc::clone(&pool), true);
    let _store2 = SqlEntityStore::new(Arc::clone(&pool), true);
    let _store3 = SqlEntityStore::new(Arc::clone(&pool), true);

    assert_eq!(
        pool.writer_task_spawn_count(),
        1,
        "N stores constructed over one pool must spawn the writer task \
         exactly once — one writer task per pool (per DB file), not one \
         per store"
    );
}

/// Full-slice single-writer guarantee (ADR-067 Component A, Fork C slice 2):
/// with every MIGRATE-listed write path routed through the writer task, drive
/// CONCURRENT writes across entity, note, and graph stores (entries 2/3/6 —
/// er, 1/2/3) plus `SqlBridge`'s unmigrated `writer()` (entry 8, still routes
/// its self-contained `execute_batch` through the task per entry 10) over ONE
/// pool. Asserts exactly one writer task is ever spawned and every write
/// actually lands, proving the migrated paths coexist over a single DB file
/// without contending at `BEGIN IMMEDIATE` or racing the writer task's own
/// spawn-once guarantee.
///
/// Entry 9 (`SqlBridge::begin_tx`, formerly a genuine design fork on its own
/// standalone connection) is retired as of ADR-099 D5 — `begin_tx` and
/// `SqlTransaction` no longer exist on `SqlAccess` (their one production
/// caller, session-mirror ingest, converted to `atomic_unit`), so this test
/// no longer exercises it.
///
/// Constructed via a `PoolConfig` literal (`write_queue_enabled: true`), not
/// the `KHIVE_WRITE_QUEUE` env var — see the sibling
/// `upsert_entities_routes_through_writer_task_when_flag_enabled` test's doc
/// comment for the race this avoids.
#[tokio::test]
async fn concurrent_writes_across_all_migrated_stores_share_one_writer_task() {
    use crate::stores::graph::SqlGraphStore;
    use crate::stores::note::SqlNoteStore;
    use khive_storage::note::Note;
    use khive_storage::types::{Edge, SqlStatement, SqlValue};
    use khive_storage::{GraphStore as _, NoteStore as _, SqlAccess as _};
    use khive_types::EdgeRelation;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("write_queue_all_paths_shared_writer.db");
    let pool_cfg = PoolConfig {
        path: Some(path.clone()),
        write_queue_enabled: true,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(pool_cfg).unwrap());
    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(ENTITIES_DDL).unwrap();
        crate::stores::note::ensure_notes_schema(writer.conn()).unwrap();
        crate::stores::graph::ensure_graph_schema(writer.conn()).unwrap();
    }

    let entity_store = Arc::new(SqlEntityStore::new(Arc::clone(&pool), true));
    let note_store = Arc::new(SqlNoteStore::new(Arc::clone(&pool), true));
    let graph_store = Arc::new(SqlGraphStore::new_scoped(
        Arc::clone(&pool),
        true,
        "default",
    ));
    let bridge = crate::sql_bridge::SqlBridge::new(Arc::clone(&pool), true);

    assert_eq!(
        pool.writer_task_spawn_count(),
        1,
        "entity + note + graph stores plus SqlBridge over one pool must still \
         share exactly one writer task"
    );

    let entity = make_entity("default", "concept", "WriterTaskConcurrency");
    let entity_id = entity.id;

    let note = Note::new("default", "observation", "concurrent writer task note");
    let note_id = note.id;

    let edge_src = Uuid::new_v4();
    let edge_tgt = Uuid::new_v4();
    let now = chrono::Utc::now();
    let edge = Edge {
        id: Uuid::new_v4().into(),
        namespace: "default".to_string(),
        source_id: edge_src,
        target_id: edge_tgt,
        relation: EdgeRelation::Extends,
        weight: 0.9,
        created_at: now,
        updated_at: now,
        deleted_at: None,
        metadata: None,
        target_backend: None,
    };
    let edge_id = edge.id;

    // Entry 10 (SqliteWriter::execute_batch, migrated): a raw INSERT issued
    // through SqlBridge's file-backed writer() handle.
    let batch_row_id = Uuid::new_v4();
    let batch_src = Uuid::new_v4();
    let batch_tgt = Uuid::new_v4();
    let now_micros = chrono::Utc::now().timestamp_micros();
    let insert_stmt = SqlStatement {
        sql: "INSERT INTO graph_edges (namespace, id, source_id, target_id, relation, \
              weight, created_at, updated_at, deleted_at, metadata, target_backend) \
              VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, NULL, NULL)"
            .to_string(),
        params: vec![
            SqlValue::Text("default".to_string()),
            SqlValue::Text(batch_row_id.to_string()),
            SqlValue::Text(batch_src.to_string()),
            SqlValue::Text(batch_tgt.to_string()),
            SqlValue::Text("extends".to_string()),
            SqlValue::Float(0.5),
            SqlValue::Integer(now_micros),
            SqlValue::Integer(now_micros),
        ],
        label: Some("test_execute_batch".to_string()),
    };

    let entity_fut = {
        let entity_store = Arc::clone(&entity_store);
        async move { entity_store.upsert_entity(entity).await }
    };
    let note_fut = {
        let note_store = Arc::clone(&note_store);
        async move { note_store.upsert_note(note).await }
    };
    let edge_fut = {
        let graph_store = Arc::clone(&graph_store);
        async move { graph_store.upsert_edge(edge).await }
    };
    let batch_fut = async {
        let mut writer = bridge.writer().await.unwrap();
        writer.execute_batch(vec![insert_stmt]).await
    };

    let (entity_res, note_res, edge_res, batch_res) =
        tokio::join!(entity_fut, note_fut, edge_fut, batch_fut);

    entity_res.unwrap();
    note_res.unwrap();
    edge_res.unwrap();
    batch_res.unwrap();

    assert!(entity_store.get_entity(entity_id).await.unwrap().is_some());
    assert!(note_store.get_note(note_id).await.unwrap().is_some());
    assert!(graph_store.get_edge(edge_id).await.unwrap().is_some());

    assert_eq!(
        pool.writer_task_spawn_count(),
        1,
        "concurrent writes across every migrated path must not trigger a \
         second writer task spawn"
    );
}

// =============================================================================
// Fork C slice 2: single-row `upsert_entity` (via `with_writer`) routes
// through the WriterTask when the flag is on
// =============================================================================

/// Proves `upsert_entity` (the SINGLE-row path, going through the
/// `with_writer` helper directly rather than the already-migrated
/// `upsert_entities` batch path) is actually enqueued on the pool's shared
/// `WriterTaskHandle` channel when `KHIVE_WRITE_QUEUE=1` — not inferred from
/// wall-clock contention (real SQLite file-level locking would serialize a
/// competing writer connection against an in-flight transaction regardless
/// of which Rust-level path issued it, which makes elapsed-time alone a
/// vacuous signal here), but read directly off `WriterTaskHandle::queue_depth`,
/// the live gauge over the exact `mpsc` channel `with_writer`'s writer-task
/// branch must call `send` on.
///
/// Technique: submit an occupier request directly on the SAME
/// `WriterTaskHandle` the store resolved at construction. The occupier signals
/// a oneshot once it is actually running inside the writer task's single
/// drain slot, then parks on a second oneshot (`blocking_recv`, valid here
/// because this closure runs inside the writer task's `spawn_blocking`) until
/// the test releases it — deterministically holding that one drain slot open
/// for as long as the test needs, no sleep/timing race. While the slot is
/// held, call `store.upsert_entity` on a separate task and poll
/// `queue_depth()`: a version that genuinely routes this call through
/// `writer_task.send(..)` must show `queue_depth() >= 1` (the request sitting
/// in the channel behind the still-running occupier) before the occupier is
/// released; a version that left `upsert_entity`/`with_writer` on the legacy
/// `pool.try_writer()` path never touches this channel at all, so
/// `queue_depth()` would stay `0` for the full poll window — this is what
/// makes the test non-vacuous (verified: reverting `with_writer` to always
/// take the legacy branch makes this assertion fail, see khive-db PR history
/// for Fork C slice 2).
///
/// Constructed via a `PoolConfig` literal (`write_queue_enabled: true`), not
/// the `KHIVE_WRITE_QUEUE` env var — see
/// `upsert_entities_routes_through_writer_task_when_flag_enabled`'s doc
/// comment for the race this avoids.
#[tokio::test]
async fn upsert_entity_routes_through_writer_task_when_flag_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("write_queue_entity_single.db");
    let pool_cfg = PoolConfig {
        path: Some(path.clone()),
        write_queue_enabled: true,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(pool_cfg).unwrap());
    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(ENTITIES_DDL).unwrap();
    }

    let store = Arc::new(SqlEntityStore::new(Arc::clone(&pool), true));

    let writer_task = pool
        .writer_task_handle()
        .unwrap()
        .expect("writer task must be spawned with the flag on for a file-backed pool");

    let (started_tx, started_rx) = oneshot::channel::<()>();
    let (release_tx, release_rx) = oneshot::channel::<()>();
    let occupier = {
        let writer_task = writer_task.clone();
        tokio::spawn(async move {
            writer_task
                .send(move |_conn| {
                    let _ = started_tx.send(());
                    let _ = release_rx.blocking_recv();
                    Ok::<(), StorageError>(())
                })
                .await
        })
    };

    started_rx
        .await
        .expect("occupier must signal it has started running inside the writer task");
    assert_eq!(
        writer_task.queue_depth(),
        0,
        "channel must start empty once the occupier has been dequeued and is running"
    );

    let entity = make_entity("default", "concept", "RoPE");
    let entity_id = entity.id;

    let store_task = {
        let store = Arc::clone(&store);
        tokio::spawn(async move { store.upsert_entity(entity).await })
    };

    let mut saw_enqueued = false;
    for _ in 0..100 {
        if writer_task.queue_depth() >= 1 {
            saw_enqueued = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        saw_enqueued,
        "upsert_entity's write request never appeared in the writer task's \
         channel while the occupier held the single drain slot — with_writer \
         is not routing this single-row write through the shared writer task"
    );

    release_tx
        .send(())
        .expect("occupier must still be waiting on the release signal");
    occupier
        .await
        .expect("occupier task must not panic")
        .expect("occupier write must succeed");
    store_task
        .await
        .expect("store task must not panic")
        .expect("upsert_entity must succeed once unblocked");

    let fetched = store.get_entity(entity_id).await.unwrap();
    assert!(
        fetched.is_some(),
        "entity must be committed and readable after queuing behind the occupier"
    );
    assert_eq!(fetched.unwrap().name, "RoPE");
}

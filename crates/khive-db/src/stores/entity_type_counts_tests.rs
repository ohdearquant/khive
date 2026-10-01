use super::*;
use crate::pool::PoolConfig;
use std::collections::BTreeMap;

fn store() -> SqlEntityStore {
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: None,
            ..PoolConfig::default()
        })
        .unwrap(),
    );
    pool.writer()
        .unwrap()
        .conn()
        .execute_batch(ENTITIES_DDL)
        .unwrap();
    SqlEntityStore::new(pool, false)
}

async fn seed(store: &SqlEntityStore, namespace: &str, entity_type: Option<&str>, deleted: bool) {
    let mut entity =
        Entity::new(namespace, "concept", "GroupedTypeFixture").with_entity_type(entity_type);
    entity.deleted_at = deleted.then_some(1);
    store.upsert_entity(entity).await.unwrap();
}

#[tokio::test]
async fn entity_type_counts_cover_large_duplicate_and_empty_namespace_sets() {
    let store = store();
    let mut namespaces: Vec<String> = (0..501).map(|index| format!("scope:{index}")).collect();
    namespaces.extend([
        "scope:0".into(),
        "scope:0".into(),
        "scope:\"quoted\"".into(),
    ]);
    seed(&store, "scope:0", Some("algorithm"), false).await;
    seed(&store, "scope:500", Some("algorithm"), false).await;
    seed(&store, "scope:250", None, false).await;
    seed(&store, "scope:\"quoted\"", Some("quoted"), false).await;
    seed(&store, "scope:0", Some("algorithm"), true).await;
    seed(&store, "scope:250", None, true).await;
    seed(&store, "scope:hidden", Some("hidden"), false).await;
    let groups = store
        .count_entities_by_type(&namespaces)
        .await
        .unwrap()
        .expect("supported report");
    assert_eq!(groups.iter().map(|(_, count)| count).sum::<u64>(), 4);
    assert_eq!(
        groups.into_iter().collect::<BTreeMap<_, _>>(),
        BTreeMap::from([
            (None, 1),
            (Some("algorithm".into()), 2),
            (Some("quoted".into()), 1)
        ])
    );
    assert_eq!(
        store.count_entities_by_type(&[]).await.unwrap(),
        Some(Vec::new())
    );
}

#[tokio::test]
async fn entity_type_counts_preserve_null_empty_and_case_sensitive_historical_labels() {
    let store = store();
    for entity_type in [
        None,
        Some(""),
        Some("null"),
        Some("<null>"),
        Some("\\<null>"),
        Some("Z"),
        Some("z"),
        Some("Ä"),
    ] {
        seed(&store, "target", entity_type, false).await;
    }
    store
        .upsert_entity(
            Entity::new("target", "concept", "LegacyPropertyType")
                .with_properties(serde_json::json!({"type": "algorithm"})),
        )
        .await
        .unwrap();
    let groups = store
        .count_entities_by_type(&["target".into()])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(groups.len(), 8);
    assert_eq!(
        groups.into_iter().collect::<BTreeMap<_, _>>(),
        BTreeMap::from([
            (None, 2),
            (Some("".into()), 1),
            (Some("null".into()), 1),
            (Some("<null>".into()), 1),
            (Some("\\<null>".into()), 1),
            (Some("Z".into()), 1),
            (Some("z".into()), 1),
            (Some("Ä".into()), 1)
        ])
    );
}

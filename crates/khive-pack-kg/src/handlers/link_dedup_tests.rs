use crate::KgPack;
use khive_runtime::{KhiveRuntime, Namespace, NamespaceToken, VerbRegistry, VerbRegistryBuilder};
use serde_json::{json, Value};

/// A concept pair linked with `competes_with` in both directions. One of the
/// two entries always runs from the larger id to the smaller one, so the
/// duplicate-detection key has to put both entries in the same order.
async fn reversed_symmetric_links() -> (KgPack, NamespaceToken, VerbRegistry, Value) {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    let concept_a = rt
        .create_entity_with_embedding_report(&token, "concept", None, "A", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .expect("create concept a");
    let concept_b = rt
        .create_entity_with_embedding_report(&token, "concept", None, "B", None, None, vec![])
        .await
        .map(|(record, _report)| record)
        .expect("create concept b");

    let pack = KgPack::new(rt.clone());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    let registry = builder.build().expect("kg registry builds");
    rt.install_edge_rules(registry.all_edge_rules());

    let links = json!([
        {
            "source_id": concept_a.id.to_string(),
            "target_id": concept_b.id.to_string(),
            "relation": "competes_with",
        },
        {
            "source_id": concept_b.id.to_string(),
            "target_id": concept_a.id.to_string(),
            "relation": "competes_with",
        },
    ]);
    (pack, token, registry, links)
}

#[tokio::test]
async fn bulk_link_atomic_mode_skips_reversed_symmetric_duplicate() {
    let (pack, token, registry, links) = reversed_symmetric_links().await;

    let response = pack
        .handle_link(&token, json!({"links": links, "atomic": true}), &registry)
        .await
        .expect("atomic bulk link of a reversed symmetric pair succeeds");

    assert_eq!(response["skipped"], 1);
    assert_eq!(response["created"], 1);
}

#[tokio::test]
async fn non_atomic_bulk_link_skips_reversed_symmetric_duplicate() {
    let (pack, token, registry, links) = reversed_symmetric_links().await;

    let response = pack
        .handle_link(&token, json!({"links": links, "atomic": false}), &registry)
        .await
        .expect("non-atomic bulk link of a reversed symmetric pair succeeds");

    assert_eq!(response["skipped"], 1);
    assert_eq!(response["created"], 1);
}

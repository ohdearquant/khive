use super::*;
use std::sync::Arc;

use khive_pack_kg::KgPack;
use khive_runtime::operations::arm_fts_fail_scoped;
use khive_runtime::{Namespace, VerbRegistryBuilder};
use khive_storage::types::{DeleteMode, PageRequest};
use khive_storage::EventFilter;
use khive_types::{EventKind, SubstrateKind};

fn fixture() -> (KhiveRuntime, NamespaceToken) {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(crate::ToolPack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");
    registry.apply_schema_plans(rt.backend());
    rt.install_edge_rules(registry.all_edge_rules());
    let namespace = Namespace::parse(&format!("tool-claim-{}", Uuid::new_v4().as_simple()))
        .expect("test namespace");
    let token = rt.authorize(namespace).expect("namespace token");
    (rt, token)
}

fn spec(name: &str, capabilities: &[&str]) -> RegisterSpec {
    RegisterSpec {
        name: name.into(),
        kind: "tool".into(),
        description: None,
        schema: None,
        source: None,
        side_effect: "read".into(),
        trust: "first_party".into(),
        capabilities: capabilities.iter().map(|name| (*name).into()).collect(),
        tags: vec![],
    }
}

#[tokio::test]
async fn concurrent_registry_claim_has_one_winner_and_one_row() {
    let (rt, token) = fixture();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let left = BEFORE_CLAIM.scope(
        Arc::clone(&barrier),
        register_one(&rt, &token, spec("Shared Name", &[])),
    );
    let right = BEFORE_CLAIM.scope(barrier, register_one(&rt, &token, spec("shared name", &[])));
    let (left, right) = tokio::join!(left, right);
    let left = left.expect("first registration");
    let right = right.expect("second registration");
    assert_eq!(left.1 as u8 + right.1 as u8, 1);
    assert_eq!(left.0.id, right.0.id);
    assert_eq!(left.0.name, right.0.name);
    let rows = rt
        .list_entities_tagged(
            &token,
            Some(REGISTRY_ENTITY_KIND),
            Some(REGISTRY_TAG),
            10,
            0,
        )
        .await
        .expect("registry rows");
    assert_eq!(rows.len(), 1);
}

#[tokio::test]
async fn concurrent_tools_share_one_new_capability_and_two_edges() {
    let (rt, token) = fixture();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let left = BEFORE_CLAIM.scope(
        Arc::clone(&barrier),
        register_one(&rt, &token, spec("first tool", &["Shared Capability"])),
    );
    let right = BEFORE_CLAIM.scope(
        barrier,
        register_one(&rt, &token, spec("second tool", &["shared capability"])),
    );
    let (left, right) = tokio::join!(left, right);
    let left = left.expect("first tool");
    let right = right.expect("second tool");
    let left_cap = left.2[0]["id"].as_str().expect("first capability id");
    let right_cap = right.2[0]["id"].as_str().expect("second capability id");
    assert_eq!(left_cap, right_cap);
    let capabilities = rt
        .list_entities_tagged(&token, Some("concept"), Some(CAPABILITY_TAG), 10, 0)
        .await
        .expect("capability rows");
    assert_eq!(capabilities.len(), 1);
    for tool in [&left.0, &right.0] {
        let edges = rt
            .neighbors(
                &token,
                tool.id,
                Direction::Out,
                Some(10),
                Some(vec![EdgeRelation::Implements]),
            )
            .await
            .expect("implements edge");
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].node_id.to_string(), left_cap);
    }
}

#[tokio::test]
async fn failed_post_claim_index_keeps_id_and_retry_repairs_suggest() {
    let (rt, token) = fixture();
    let name = "uniquely repairable scanner";
    let id = derived_registry_id("object", token.namespace().as_str(), name);
    let guard = arm_fts_fail_scoped(token.namespace().as_str());
    let error = register_one(&rt, &token, spec(name, &[]))
        .await
        .expect_err("indexing must fail after the claim");
    assert!(error.to_string().contains(&id.to_string()), "{error}");
    drop(guard);
    let row = rt
        .get_entity(&token, id)
        .await
        .expect("claimed row survives");
    assert_eq!(row.name, name);
    let before = suggest(&rt, &token, json!({"query": name}))
        .await
        .expect("suggest before repair");
    assert_eq!(before["count"], json!(0));

    let (repaired, created, _) = register_one(&rt, &token, spec(name, &[]))
        .await
        .expect("registration retries indexing");
    assert!(!created);
    assert_eq!(repaired.id, id);
    let after = suggest(&rt, &token, json!({"query": name}))
        .await
        .expect("suggest after repair");
    assert!(
        after["results"]
            .as_array()
            .expect("results")
            .iter()
            .any(|hit| hit["full_id"] == id.to_string()),
        "{after}"
    );
}

#[tokio::test]
async fn minted_legacy_id_wins_and_derived_tombstone_refuses() {
    let (rt, token) = fixture();
    let legacy = rt
        .create_entity_with_embedding_report(
            &token,
            REGISTRY_ENTITY_KIND,
            Some("tool"),
            "legacy row",
            None,
            None,
            vec![REGISTRY_TAG.into(), "tool".into()],
        )
        .await
        .map(|(row, _report)| row)
        .expect("legacy row");
    let (found, created, _) = register_one(&rt, &token, spec("LEGACY ROW", &[]))
        .await
        .expect("re-registration");
    assert!(!created);
    assert_eq!(found.id, legacy.id);
    let derived = derived_registry_id("object", token.namespace().as_str(), "legacy row");
    assert!(rt
        .get_entity_including_deleted(&token, derived)
        .await
        .expect("derived lookup")
        .is_none());

    let (tombstoned, _, _) = register_one(&rt, &token, spec("deleted row", &[]))
        .await
        .expect("new row");
    rt.entities(&token)
        .expect("entity store")
        .delete_entity(tombstoned.id, DeleteMode::Soft)
        .await
        .expect("soft delete");
    let error = register_one(&rt, &token, spec("deleted row", &[]))
        .await
        .expect_err("tombstone cannot be revived");
    assert!(error.to_string().contains(&tombstoned.id.to_string()));
    assert!(rt
        .get_entity_including_deleted(&token, tombstoned.id)
        .await
        .expect("tombstone lookup")
        .expect("tombstone")
        .deleted_at
        .is_some());
}

#[tokio::test]
async fn deleted_legacy_id_blocks_same_name_registration() {
    let (rt, token) = fixture();
    let name = "deleted legacy row";
    let legacy = rt
        .create_entity_with_embedding_report(
            &token,
            REGISTRY_ENTITY_KIND,
            Some("tool"),
            name,
            None,
            None,
            vec![REGISTRY_TAG.into(), "tool".into()],
        )
        .await
        .map(|(row, _report)| row)
        .expect("legacy row");
    let derived = derived_registry_id("object", token.namespace().as_str(), name);
    assert_ne!(legacy.id, derived);
    rt.entities(&token)
        .expect("entity store")
        .delete_entity(legacy.id, DeleteMode::Soft)
        .await
        .expect("soft delete legacy row");

    let error = register_one(&rt, &token, spec("DELETED LEGACY ROW", &[]))
        .await
        .expect_err("legacy tombstone must block a new identity");
    assert!(
        error.to_string().contains(&legacy.id.to_string()),
        "{error}"
    );
    assert!(rt
        .get_entity_including_deleted(&token, derived)
        .await
        .expect("derived lookup")
        .is_none());
}

#[tokio::test]
async fn tombstones_with_other_tag_or_namespace_do_not_hold_registry_name() {
    let (rt, token) = fixture();
    let other_tag = rt
        .create_entity_with_embedding_report(
            &token,
            REGISTRY_ENTITY_KIND,
            Some("tool"),
            "unrelated tag",
            None,
            None,
            vec!["unrelated".into()],
        )
        .await
        .map(|(row, _report)| row)
        .expect("other-tag row");
    rt.entities(&token)
        .expect("entity store")
        .delete_entity(other_tag.id, DeleteMode::Soft)
        .await
        .expect("delete other-tag row");
    let (_, created, _) = register_one(&rt, &token, spec("UNRELATED TAG", &[]))
        .await
        .expect("other tag is not registry ownership");
    assert!(created);

    let other_token = rt
        .authorize(Namespace::parse("foreign-tool-claims").expect("other namespace"))
        .expect("other token");
    let foreign = rt
        .create_entity_with_embedding_report(
            &other_token,
            REGISTRY_ENTITY_KIND,
            Some("tool"),
            "foreign tombstone",
            None,
            None,
            vec![REGISTRY_TAG.into(), "tool".into()],
        )
        .await
        .map(|(row, _report)| row)
        .expect("foreign row");
    rt.entities(&other_token)
        .expect("foreign entity store")
        .delete_entity(foreign.id, DeleteMode::Soft)
        .await
        .expect("delete foreign row");
    let (_, created, _) = register_one(&rt, &token, spec("FOREIGN TOMBSTONE", &[]))
        .await
        .expect("other namespace cannot hold the name");
    assert!(created);
}

#[tokio::test]
async fn deleted_legacy_capability_blocks_same_name_claim() {
    let (rt, token) = fixture();
    let name = "legacy capability tombstone";
    let legacy = rt
        .create_entity_with_embedding_report(
            &token,
            "concept",
            Some("capability"),
            name,
            None,
            None,
            vec![CAPABILITY_TAG.into()],
        )
        .await
        .map(|(row, _report)| row)
        .expect("legacy capability");
    let derived = derived_registry_id("capability", token.namespace().as_str(), name);
    assert_ne!(legacy.id, derived);
    rt.entities(&token)
        .expect("entity store")
        .delete_entity(legacy.id, DeleteMode::Soft)
        .await
        .expect("soft delete capability");

    let error = ensure_capability(&rt, &token, "LEGACY CAPABILITY TOMBSTONE")
        .await
        .expect_err("legacy capability tombstone must block a new identity");
    assert!(
        error.to_string().contains(&legacy.id.to_string()),
        "{error}"
    );
    assert!(rt
        .get_entity_including_deleted(&token, derived)
        .await
        .expect("derived lookup")
        .is_none());
}

#[tokio::test]
async fn retry_repairs_claimed_row_without_create_event() {
    let (rt, token) = fixture();
    let name = "missing create event";
    let id = derived_registry_id("object", token.namespace().as_str(), name);
    let mut row = Entity::new(token.namespace().as_str(), REGISTRY_ENTITY_KIND, name)
        .with_entity_type(Some("tool"))
        .with_tags(vec![REGISTRY_TAG.into(), "tool".into()]);
    row.id = id;
    assert!(rt
        .entities(&token)
        .expect("entity store")
        .insert_entity_if_absent(row)
        .await
        .expect("seed row left by failed append"));
    let events = rt.events(&token).expect("event store");
    let filter = EventFilter {
        target_id: Some(id),
        kinds: vec![EventKind::EntityCreated],
        verbs: vec!["create".into()],
        substrates: vec![SubstrateKind::Entity],
        ..EventFilter::default()
    };
    let page = PageRequest {
        offset: 0,
        limit: 10,
    };
    assert!(events
        .query_events(filter.clone(), page.clone())
        .await
        .expect("event read before retry")
        .items
        .is_empty());

    let (registered, created, _) = register_one(&rt, &token, spec(name, &[]))
        .await
        .expect("retry completes the claim");
    assert_eq!(registered.id, id);
    assert!(!created);
    let emitted = events
        .query_events(filter.clone(), page.clone())
        .await
        .expect("event read after retry");
    assert_eq!(emitted.items.len(), 1);
    assert_eq!(emitted.items[0].payload["id"], json!(id));

    register_one(&rt, &token, spec(name, &[]))
        .await
        .expect("idempotent registration");
    let repeated = events
        .query_events(filter, page)
        .await
        .expect("event read after another retry");
    assert_eq!(repeated.items.len(), 1);
}

#[tokio::test]
async fn exact_capability_lookup_finds_oldest_beyond_five_thousand() {
    let (rt, token) = fixture();
    let mut entities = Vec::with_capacity(5001);
    let mut oldest = Entity::new(token.namespace().as_str(), "concept", "old capability")
        .with_entity_type(Some("capability"))
        .with_tags(vec![CAPABILITY_TAG.into()]);
    oldest.created_at = 1;
    let oldest_id = oldest.id;
    entities.push(oldest);
    for index in 0..5000 {
        entities.push(
            Entity::new(
                token.namespace().as_str(),
                "concept",
                format!("filler capability {index}"),
            )
            .with_entity_type(Some("capability"))
            .with_tags(vec![CAPABILITY_TAG.into()]),
        );
    }
    rt.entities(&token)
        .expect("entity store")
        .upsert_entities(entities)
        .await
        .expect("seed capabilities");
    let found = ensure_capability(&rt, &token, "OLD CAPABILITY")
        .await
        .expect("exact query finds old row");
    assert_eq!(found.id, oldest_id);
    let count = rt
        .count_entities_tagged(&token, Some("concept"), Some(CAPABILITY_TAG))
        .await
        .expect("count capabilities");
    assert_eq!(count, 5001);
}

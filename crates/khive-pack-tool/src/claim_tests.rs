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
async fn unchanged_registration_cannot_overwrite_newer_search_document() {
    use khive_storage::types::{TextFilter, TextQueryMode, TextSearchRequest};

    let (rt, token) = fixture();
    let initial = register(
        &rt,
        &token,
        json!({"name":"indexed tool", "description":"oldquartz"}),
    )
    .await
    .unwrap();
    let id = initial["tool"]["full_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let text = rt.text(&token).unwrap();
    let before = text
        .get_document(token.namespace().as_str(), id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(before.body, "indexed tool oldquartz");
    let ready = Arc::new(tokio::sync::Barrier::new(2));
    let resume = Arc::new(tokio::sync::Notify::new());
    let stale = BEFORE_REGISTRATION_REINDEX.scope(
        (Arc::clone(&ready), Arc::clone(&resume)),
        register(&rt, &token, json!({"name":"indexed tool"})),
    );
    let newer = async {
        ready.wait().await;
        let response = register(
            &rt,
            &token,
            json!({"name":"indexed tool", "description":"newcobalt"}),
        )
        .await
        .unwrap();
        assert_eq!(response["updated"], true);
        let row = rt.get_entity(&token, id).await.unwrap();
        let document = text
            .get_document(token.namespace().as_str(), id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(document.body, "indexed tool newcobalt");
        resume.notify_one();
        (row, document)
    };
    let (stale, (winner, document)) =
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(stale, newer)
        })
        .await
        .expect("both registration paths settle after the observed capture");
    let after = text
        .get_document(token.namespace().as_str(), id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(&after).unwrap(),
        serde_json::to_value(&document).unwrap()
    );
    for (query, expected) in [("oldquartz", vec![]), ("newcobalt", vec![id])] {
        let hits = text
            .search(TextSearchRequest {
                query: query.into(),
                mode: TextQueryMode::Plain,
                filter: Some(TextFilter {
                    ids: vec![id],
                    namespaces: vec![token.namespace().to_string()],
                    ..Default::default()
                }),
                top_k: 10,
                snippet_chars: 0,
            })
            .await
            .unwrap();
        assert_eq!(
            hits.iter().map(|hit| hit.subject_id).collect::<Vec<_>>(),
            expected
        );
    }
    let error = stale.expect_err("a captured old row cannot replace the winner's text index");
    assert!(error.to_string().contains(&id.to_string()), "{error}");
    assert!(
        error.to_string().contains("changed concurrently"),
        "{error}"
    );
    assert_eq!(rt.get_entity(&token, id).await.unwrap(), winner);
    let retry = register(&rt, &token, json!({"name":"indexed tool"}))
        .await
        .unwrap();
    assert_eq!(retry["unchanged"], true);
    assert_eq!(rt.get_entity(&token, id).await.unwrap(), winner);
    assert_eq!(
        serde_json::to_value(
            text.get_document(token.namespace().as_str(), id)
                .await
                .unwrap()
                .unwrap()
        )
        .unwrap(),
        serde_json::to_value(document).unwrap(),
    );
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
async fn concurrent_register_claim_loser_applies_its_supplied_definition() {
    let (rt, token) = fixture();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    // Both initial claim repairs finish before the loser changes the row.
    // Otherwise the winner's repair can correctly refuse a stale revision.
    let repaired = Arc::new(tokio::sync::Barrier::new(2));
    let left = AFTER_CLAIM.scope(
        Arc::clone(&repaired),
        BEFORE_CLAIM.scope(
            Arc::clone(&barrier),
            register(
                &rt,
                &token,
                json!({"name":"shared update", "source":"mcp:left"}),
            ),
        ),
    );
    let right = AFTER_CLAIM.scope(
        repaired,
        BEFORE_CLAIM.scope(
            barrier,
            register(
                &rt,
                &token,
                json!({"name":"SHARED UPDATE", "source":"mcp:right"}),
            ),
        ),
    );
    let (left, right) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(left, right)
    })
    .await
    .expect("both registrations settle");
    let left = left.unwrap();
    let right = right.unwrap();
    assert_ne!(left["created"], right["created"]);
    assert_eq!(left["tool"]["full_id"], right["tool"]["full_id"]);
    let (winner, loser, source) = if left["created"] == true {
        (&left, &right, "mcp:right")
    } else {
        (&right, &left, "mcp:left")
    };
    assert_eq!(winner["updated"], false);
    assert_eq!(loser["updated"], true);
    assert_eq!(loser["unchanged"], false);
    let id = loser["tool"]["full_id"].as_str().unwrap().parse().unwrap();
    let row = rt.get_entity(&token, id).await.unwrap();
    assert_eq!(row.properties.as_ref().unwrap()["source"], source);
    assert_eq!(row.name, winner["tool"]["name"].as_str().unwrap());
    assert_eq!(
        rt.count_entities_tagged(&token, Some(REGISTRY_ENTITY_KIND), Some(REGISTRY_TAG))
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn concurrent_registration_updates_refuse_a_stale_snapshot_before_linking() {
    let (rt, token) = fixture();
    let initial = register(
        &rt,
        &token,
        json!({"name":"guarded", "source":"mcp:original"}),
    )
    .await
    .unwrap();
    let id = initial["tool"]["full_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let before = rt.get_entity(&token, id).await.unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(crate::ToolPack::new(rt.clone()));
    let registry = builder.build().unwrap();
    let requested = request(
        &rt,
        &registry,
        &token,
        json!({"tool":"guarded", "actor":"agent:requester"}),
    )
    .await
    .unwrap();
    decide_request(
        &rt,
        &token,
        json!({"id": requested["request_id"]}),
        "granted",
    )
    .await
    .unwrap();
    let grants_before = requests(&rt, &token, json!({})).await.unwrap();
    assert_eq!(grants_before["count"], 1);
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let resume = Arc::new(tokio::sync::Notify::new());
    let left = BEFORE_REGISTRATION_UPDATE.scope(
        (Arc::clone(&barrier), Arc::clone(&resume)),
        register(
            &rt,
            &token,
            json!({"name":"guarded", "description":"left", "capabilities":["left capability"]}),
        ),
    );
    let right = async {
        barrier.wait().await;
        let result = register(
            &rt,
            &token,
            json!({"name":"guarded", "trust":"first_party", "capabilities":["right capability"]}),
        )
        .await;
        resume.notify_one();
        result
    };
    let (left, right) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(left, right)
    })
    .await
    .expect("both guarded updates settle");
    let winner = right.expect("intervening registration commits before the stale one resumes");
    let refusal = left.expect_err("the original registration snapshot is stale");
    assert!(matches!(refusal, RuntimeError::Khive(_)), "{refusal:?}");
    assert!(
        refusal.to_string().contains("changed concurrently"),
        "{refusal}"
    );
    assert_eq!(winner["updated"], true);
    let after = rt.get_entity(&token, id).await.unwrap();
    assert_eq!(after.version, before.version + 1);
    assert_eq!(after.properties.as_ref().unwrap()["source"], "mcp:original");
    assert_eq!(after.description, before.description);
    assert_eq!(after.properties.as_ref().unwrap()["trust"], "first_party");
    assert_eq!(
        requests(&rt, &token, json!({})).await.unwrap(),
        grants_before
    );
    let capabilities = capabilities_of(&rt, &token, id).await.unwrap();
    assert_eq!(capabilities.len(), 1);
    assert_eq!(capabilities[0]["name"], "right capability");
    assert_eq!(
        rt.count_entities_tagged(&token, Some("concept"), Some(CAPABILITY_TAG))
            .await
            .unwrap(),
        1
    );
    let retry = register(
        &rt,
        &token,
        json!({"name":"guarded", "description":"left", "capabilities":["left capability"]}),
    )
    .await
    .unwrap();
    assert_eq!(retry["updated"], true);
    let retried = rt.get_entity(&token, id).await.unwrap();
    assert_eq!(retried.description.as_deref(), Some("left"));
    assert_eq!(retried.properties.as_ref().unwrap()["trust"], "first_party");
    assert_eq!(capabilities_of(&rt, &token, id).await.unwrap().len(), 2);
    assert_eq!(
        requests(&rt, &token, json!({})).await.unwrap(),
        grants_before
    );
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

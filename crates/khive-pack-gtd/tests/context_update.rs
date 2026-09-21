//! Context references retain creation's entity and namespace contract on update.

mod common;

use common::{assign, pack, rt, Fixture};
use khive_runtime::{Namespace, RuntimeError};
use serde_json::{json, Value};
use uuid::Uuid;

async fn context(fixture: &Fixture, name: &str, namespace: &str) -> String {
    fixture
        .dispatch(
            "create",
            json!({"kind": "concept", "name": name, "namespace": namespace,
                   "skip_dedup_check": true}),
        )
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn context_update_refuses_invalid_references_without_changing_the_task() {
    let fixture = pack(rt());
    let anchor = context(&fixture, "initial anchor", "local").await;
    let foreign = context(&fixture, "foreign anchor", "foreign").await;
    let deleted = context(&fixture, "deleted anchor", "local").await;
    fixture
        .dispatch("delete", json!({"id": deleted}))
        .await
        .unwrap();
    let task = assign(
        &fixture,
        json!({"title": "anchored task", "context_entity_id": anchor}),
    )
    .await;
    let id = &task["full_id"];
    let before = fixture.dispatch("get", json!({"id": id})).await.unwrap();
    for (value, not_found) in [
        (json!("not-a-uuid"), false),
        (json!("deadbeef"), false),
        (json!(17), false),
        (json!({"id": anchor}), false),
        (id.clone(), false),
        (json!(Uuid::new_v4().to_string()), true),
        (json!(deleted), true),
        (json!(foreign), true),
    ] {
        let error = fixture
            .dispatch(
                "update",
                json!({"id": id, "content": "must not persist",
                "properties": {"context_entity_id": value, "unrelated": "must not persist"}}),
            )
            .await
            .expect_err("invalid context must be refused before storage");
        assert!(error.to_string().contains("context_entity_id"), "{error}");
        if not_found {
            assert!(matches!(error, RuntimeError::NotFound(_)), "{error}");
        } else {
            assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error}");
        }
        assert_eq!(
            fixture.dispatch("get", json!({"id": id})).await.unwrap(),
            before
        );
    }
}

#[tokio::test]
async fn context_update_canonicalizes_and_preserves_omit_and_clear_semantics() {
    let fixture = pack(rt());
    let first = context(&fixture, "old anchor", "local").await;
    let second = context(&fixture, "new anchor", "local").await;
    let task = assign(
        &fixture,
        json!({"title": "moving task", "context_entity_id": first}),
    )
    .await;
    let id = &task["full_id"];
    let parsed = Uuid::parse_str(&second).unwrap();
    for spelling in [
        second.clone(),
        second.to_ascii_uppercase(),
        parsed.simple().to_string(),
        parsed.urn().to_string(),
        parsed.braced().to_string(),
    ] {
        fixture
            .dispatch(
                "update",
                json!({"id": id, "properties": {"context_entity_id": spelling}}),
            )
            .await
            .unwrap();
        let stored = fixture.dispatch("get", json!({"id": id})).await.unwrap();
        assert_eq!(stored["properties"]["context_entity_id"], second);
    }
    assert_eq!(
        fixture
            .dispatch("gtd.tasks", json!({"context_entity_id": first}))
            .await
            .unwrap(),
        json!([])
    );
    let matches = fixture
        .dispatch("gtd.tasks", json!({"context_entity_id": second}))
        .await
        .unwrap();
    assert_eq!(matches.as_array().unwrap().len(), 1);
    assert_eq!(matches[0]["full_id"], *id);

    // Omission does not revalidate or repair an existing reference after deletion.
    fixture
        .dispatch("delete", json!({"id": second}))
        .await
        .unwrap();
    fixture
        .dispatch(
            "update",
            json!({"id": id, "properties": {"unrelated": "allowed"}}),
        )
        .await
        .unwrap();
    assert_eq!(
        fixture.dispatch("get", json!({"id": id})).await.unwrap()["properties"]
            ["context_entity_id"],
        second
    );
    fixture
        .dispatch(
            "update",
            json!({"id": id, "properties": {"context_entity_id": null}}),
        )
        .await
        .unwrap();
    let cleared = fixture.dispatch("get", json!({"id": id})).await.unwrap();
    assert_eq!(
        cleared["properties"].get("context_entity_id"),
        Some(&Value::Null)
    );
    assert_eq!(cleared["properties"]["unrelated"], "allowed");
    assert_eq!(
        fixture
            .dispatch("gtd.tasks", json!({"context_entity_id": second}))
            .await
            .unwrap(),
        json!([])
    );

    let observation = fixture
        .dispatch(
            "create",
            json!({"kind": "observation", "content": "ordinary metadata"}),
        )
        .await
        .unwrap();
    fixture.dispatch("update", json!({"id": observation["id"], "properties": {"context_entity_id": "ordinary metadata"}})).await.unwrap();
    let stored = fixture
        .dispatch("get", json!({"id": observation["id"]}))
        .await
        .unwrap();
    assert_eq!(
        stored["properties"]["context_entity_id"],
        "ordinary metadata"
    );
}

#[tokio::test]
async fn context_update_rejects_visible_only_entity_at_the_shared_hook_seam() {
    let runtime = rt();
    let fixture = pack(runtime.clone());
    let foreign = context(&fixture, "visible foreign anchor", "foreign").await;
    let task = assign(&fixture, json!({"title": "local task"})).await;
    let token = runtime
        .authorize_with_visibility(
            Namespace::local(),
            vec![Namespace::parse("foreign").unwrap()],
        )
        .unwrap();
    let anchor = Uuid::parse_str(&foreign).unwrap();
    assert!(runtime.resolve(&token, anchor).await.unwrap().is_some());
    let id = Uuid::parse_str(task["full_id"].as_str().unwrap()).unwrap();
    let note = runtime
        .notes(&token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .unwrap();
    let mut args: Value =
        json!({"id": id.to_string(), "properties": {"context_entity_id": foreign}});
    let error = fixture
        .registry
        .prepare_note_update_hook(&runtime, &token, &note, &mut args)
        .await
        .unwrap_err();
    assert!(matches!(error, RuntimeError::NotFound(_)), "{error}");
    assert_eq!(
        runtime
            .notes(&token)
            .unwrap()
            .get_note(id)
            .await
            .unwrap()
            .unwrap(),
        note
    );
}

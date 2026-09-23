//! Proposal admission reserves runtime-owned properties before writing history.

use khive_runtime::{KhiveRuntime, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use khive_storage::{SqlStatement, SqlValue};
use serde_json::{json, Value};
use uuid::Uuid;

fn surface() -> (KhiveRuntime, VerbRegistry) {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        packs: vec!["kg".into()],
        brain_profile: None,
        actor_id: Some("proposal-reservation-test".into()),
        ..RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("proposal-reservation-test".into()));
    builder.register(crate::KgPack::new(runtime.clone()));
    (runtime, builder.build().expect("KG registry"))
}

async fn seed_target(registry: &VerbRegistry) -> String {
    let created = registry
        .dispatch(
            "create",
            json!({
                "kind": "concept", "name": "Proposal target",
                "properties": {"ordinary": "original"}, "skip_dedup_check": true
            }),
        )
        .await
        .expect("ordinary entity seed");
    let id = created["id"].as_str().expect("create returns an entity id");
    let parsed = Uuid::parse_str(id).expect("create returns a full UUID");
    assert_eq!(
        id,
        parsed.to_string(),
        "create returns a canonical full UUID"
    );
    id.to_owned()
}

async fn snapshot(runtime: &KhiveRuntime) -> Value {
    let mut reader = runtime.sql().reader().await.expect("snapshot reader");
    let mut domain = Vec::new();
    for sql in [
        "SELECT * FROM entities ORDER BY id",
        "SELECT * FROM notes ORDER BY id",
        "SELECT * FROM graph_edges ORDER BY id",
        "SELECT rowid, * FROM fts_entities ORDER BY rowid",
        "SELECT rowid, * FROM fts_notes ORDER BY rowid",
        "SELECT * FROM event_observations ORDER BY event_id, role, position",
    ] {
        let rows = reader
            .query_all(SqlStatement {
                sql: sql.into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("domain snapshot");
        domain.push(serde_json::to_value(rows).expect("serialize domain rows"));
    }
    let mut history = Vec::new();
    for sql in [
        "SELECT * FROM proposals_open ORDER BY proposal_id",
        // Gate audit is allowed on refusal; proposal/domain events are not.
        "SELECT * FROM events WHERE kind != 'audit' ORDER BY id",
    ] {
        let rows = reader
            .query_all(SqlStatement {
                sql: sql.into(),
                params: vec![],
                label: None,
            })
            .await
            .expect("proposal history snapshot");
        history.push(serde_json::to_value(rows).expect("serialize history rows"));
    }
    json!({"domain": domain, "history": history})
}

fn changeset(shape: &str, target: &str, properties: Value, nested_compound: bool) -> Value {
    let draft = match shape {
        "add_entity" => json!({
            "kind": "add_entity",
            "entity": {"kind": "concept", "name": "New concept", "properties": properties}
        }),
        "update_entity" => json!({
            "kind": "update_entity", "id": target, "patch": {"properties": properties}
        }),
        "add_note" => json!({
            "kind": "add_note",
            "note": {"kind": "observation", "content": "An observation", "properties": properties}
        }),
        _ => panic!("unknown fixture shape"),
    };
    if nested_compound {
        json!({"kind": "compound", "steps": [{"kind": "compound", "steps": [draft]}]})
    } else {
        draft
    }
}

fn proposal(changeset: Value) -> Value {
    json!({
        "title": "Property proposal",
        "description": "Check property admission before creating a proposal",
        "changeset": changeset
    })
}

#[tokio::test]
async fn propose_refuses_reserved_properties_without_domain_or_history_mutation() {
    let (runtime, registry) = surface();
    let target = seed_target(&registry).await;
    // Privileged fixture seed models a pre-existing forged stamp, not an admitted write.
    // The copy arm below submits the actual property map returned by get.
    {
        let mut writer = runtime.sql().writer().await.expect("legacy fixture writer");
        assert_eq!(
            writer
                .execute(SqlStatement {
                    sql: "UPDATE entities SET properties = ?1, version = version + 1 WHERE id = ?2"
                        .into(),
                    params: vec![
                        SqlValue::Text(
                            json!({"ordinary": "original", "khive:secret_gate": "copied-stamp"})
                                .to_string(),
                        ),
                        SqlValue::Text(target.clone()),
                    ],
                    label: Some("proposal_reserved_property_fixture".into()),
                })
                .await
                .expect("seed forged preimage"),
            1
        );
    }
    let existing = registry
        .dispatch("get", json!({"id": target}))
        .await
        .expect("read forged preimage");
    let copied = existing["properties"].clone();
    assert_eq!(copied["khive:secret_gate"], "copied-stamp");
    let before = snapshot(&runtime).await;
    assert!(before["history"][0].as_array().unwrap().is_empty());

    for shape in ["add_entity", "update_entity", "add_note"] {
        for (attempt, properties) in [
            ("create", json!({"khive:secret_gate": true})),
            ("replace", json!({"khive:secret_gate": "replacement-stamp"})),
            ("remove", json!({"khive:secret_gate": null})),
            ("copy", copied.clone()),
        ] {
            for nested in [false, true] {
                let error = registry
                    .dispatch(
                        "propose",
                        proposal(changeset(shape, &target, properties.clone(), nested)),
                    )
                    .await
                    .expect_err("reserved top-level property must refuse at proposal admission");
                let RuntimeError::InvalidInput(message) = error else {
                    panic!("expected shared reservation error for {shape}/{attempt}: {error:?}");
                };
                assert!(message.contains("property key `khive:secret_gate` is runtime-owned"));
                assert_eq!(
                    snapshot(&runtime).await,
                    before,
                    "{shape}/{attempt}, nested Compound={nested} must preserve all domain/history rows"
                );
            }
        }
    }
}

#[tokio::test]
async fn propose_accepts_ordinary_and_nested_reserved_spelling_properties() {
    let (runtime, registry) = surface();
    let target = seed_target(&registry).await;
    let before = snapshot(&runtime).await;
    let mut accepted = 0;
    for shape in ["add_entity", "update_entity", "add_note"] {
        for properties in [
            json!({"ordinary": "new value"}),
            json!({"ordinary": {"khive:secret_gate": "ordinary nested data"}}),
        ] {
            for nested in [false, true] {
                let expected_changeset = changeset(shape, &target, properties.clone(), nested);
                let result = registry
                    .dispatch("propose", proposal(expected_changeset.clone()))
                    .await
                    .expect("ordinary property maps remain valid proposal drafts");
                assert_eq!(result["status"], "open");
                accepted += 1;
                let mut reader = runtime.sql().reader().await.expect("event reader");
                let payload = reader
                    .query_scalar(SqlStatement {
                        sql: "SELECT payload FROM events WHERE kind = 'proposal_created' AND aggregate_id = ?1".into(),
                        params: vec![SqlValue::Text(result["id"].as_str().unwrap().into())],
                        label: None,
                    })
                    .await
                    .expect("proposal event");
                let Some(SqlValue::Text(payload)) = payload else {
                    panic!("proposal_created must retain a JSON payload");
                };
                let stored: Value = serde_json::from_str(&payload).expect("stored event JSON");
                // Compare the typed representation: optional absent fields may serialize as null.
                let expected: khive_types::ProposalChangeset =
                    serde_json::from_value(expected_changeset).expect("typed draft");
                assert_eq!(stored["changeset"], serde_json::to_value(expected).unwrap());
                drop(reader);
                let after = snapshot(&runtime).await;
                assert_eq!(
                    after["domain"], before["domain"],
                    "proposing does not apply"
                );
                assert_eq!(after["history"][0].as_array().unwrap().len(), accepted);
            }
        }
    }
}

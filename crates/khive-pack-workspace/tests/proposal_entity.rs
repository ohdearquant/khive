use khive_pack_git::GitPack;
use khive_pack_gtd::GtdPack;
use khive_pack_kg::{
    apply_worker::ProposalApplyWorker, projection_worker::ProposalsProjectionWorker, KgPack,
};
use khive_pack_session::SessionPack;
use khive_pack_workspace::WorkspacePack;
use khive_runtime::{
    KhiveRuntime, Namespace, NamespaceToken, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{EventFilter, PageRequest, TextFilter};
use khive_types::{ApplyResult, EventKind, ProposalAppliedPayload};
use serde_json::{json, Value};
use uuid::Uuid;

fn fixture() -> (KhiveRuntime, NamespaceToken, VerbRegistry) {
    let runtime = KhiveRuntime::memory().unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(GtdPack::new(runtime.clone()));
    builder.register(GitPack::new(runtime.clone()));
    builder.register(SessionPack::new(runtime.clone()));
    builder.register(WorkspacePack::new(runtime.clone()));
    // Deliberately omit the transport-installed runtime update-hook aggregate.
    // Proposal admission must use the actual applying registry.
    (runtime, token, builder.build().unwrap())
}

async fn propose_and_approve(registry: &VerbRegistry, changeset: Value) -> Uuid {
    let proposed = registry
        .dispatch(
            "propose",
            json!({
                "title": "Owner validation", "description": "Check approved entity admission",
                "changeset": changeset,
            }),
        )
        .await
        .expect("a well-formed draft may be proposed before owner validation");
    let id = Uuid::parse_str(proposed["id"].as_str().unwrap()).unwrap();
    registry
        .dispatch("review", json!({"id": id, "decision": "approve"}))
        .await
        .expect("a successful review may record a failed domain apply");
    id
}

async fn apply_results(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    id: Uuid,
) -> Vec<ApplyResult> {
    runtime
        .events(token)
        .unwrap()
        .query_events(
            EventFilter {
                kinds: vec![EventKind::ProposalApplied],
                payload_proposal_id: Some(id),
                ..Default::default()
            },
            PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap()
        .items
        .into_iter()
        .map(|event| {
            serde_json::from_value::<ProposalAppliedPayload>(event.payload)
                .unwrap()
                .result
        })
        .collect()
}

async fn assert_entity_count(runtime: &KhiveRuntime, token: &NamespaceToken, count: usize) {
    assert_eq!(
        runtime
            .list_entities(token, None, None, 100, 0)
            .await
            .unwrap()
            .len(),
        count
    );
    assert_eq!(
        runtime
            .text(token)
            .unwrap()
            .count(TextFilter::default())
            .await
            .unwrap(),
        count as u64,
        "entity rows and entity FTS must share the same apply outcome"
    );
}

#[tokio::test]
async fn proposal_workspace_matches_shared_owner_refusal_and_reverts_without_writes() {
    for wrapped in [false, true] {
        for properties in [
            None,
            Some(json!({})),
            Some(json!({"schema_version": null})),
            Some(json!({"schema_version": "1"})),
            Some(json!({"schema_version": 1.5})),
            Some(json!({"schema_version": true})),
            Some(json!({"schema_version": []})),
        ] {
            let (runtime, token, registry) = fixture();
            let mut entity = json!({"kind": "workspace", "name": "invalid workspace"});
            if let Some(properties) = properties {
                entity["properties"] = properties;
            }
            let direct_error = registry
                .dispatch("create", entity.clone())
                .await
                .expect_err("shared workspace create must reject this exact fixture");
            assert!(matches!(&direct_error, RuntimeError::InvalidInput(_)));
            let mut changeset = json!({"kind": "add_entity", "entity": entity});
            if wrapped {
                changeset = json!({"kind": "compound", "steps": [changeset]});
            }
            let id = propose_and_approve(&registry, changeset).await;
            let results = apply_results(&runtime, &token, id).await;
            assert_eq!(results.len(), 1);
            match &results[0] {
                ApplyResult::Failed {
                    error,
                    applied_step_count,
                } => {
                    assert_eq!(error, &direct_error.to_string());
                    assert_eq!(*applied_step_count, 0);
                }
                result => panic!("invalid workspace must record a failed apply: {result:?}"),
            }
            let projection = ProposalsProjectionWorker::new(runtime.clone());
            let row = projection
                .get_proposal_row(&token, id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                row.status, "approved",
                "failed pre-commit apply must revert"
            );
            assert_eq!(row.approve_count, 1);
            assert_entity_count(&runtime, &token, 0).await;

            ProposalApplyWorker::new(runtime.clone())
                .maybe_apply(&token, id, &registry, None)
                .await
                .unwrap();
            let results = apply_results(&runtime, &token, id).await;
            assert_eq!(
                results.len(),
                2,
                "direct worker retry revalidates the draft"
            );
            assert!(results.iter().all(|result| matches!(
                result,
                ApplyResult::Failed { error, applied_step_count: 0 }
                    if error == &direct_error.to_string()
            )));
            assert_entity_count(&runtime, &token, 0).await;
        }
    }
}

#[tokio::test]
async fn proposal_workspace_accepts_integer_schema_without_rewriting_approved_data() {
    for version in [json!(-1), json!(0), json!(u64::MAX)] {
        let (runtime, token, registry) = fixture();
        let properties = json!({"schema_version": version, "retained": "verbatim"});
        let id = propose_and_approve(
            &registry,
            json!({
                "kind": "add_entity",
                "entity": {
                    "kind": "  WORKSPACE  ", "name": " Approved workspace ",
                    "description": " Approved description ",
                    "properties": properties, "tags": ["retained-tag"],
                },
            }),
        )
        .await;
        let results = apply_results(&runtime, &token, id).await;
        assert_eq!(results.len(), 1);
        assert!(matches!(&results[0], ApplyResult::Success { .. }));
        let entities = runtime
            .list_entities(&token, None, None, 100, 0)
            .await
            .unwrap();
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].kind, "workspace");
        assert_eq!(entities[0].name, " Approved workspace ");
        assert_eq!(
            entities[0].description.as_deref(),
            Some(" Approved description ")
        );
        assert_eq!(entities[0].properties.as_ref(), Some(&properties));
        assert_eq!(entities[0].tags, vec!["retained-tag".to_string()]);
        let row = ProposalsProjectionWorker::new(runtime.clone())
            .get_proposal_row(&token, id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, "applied");
        assert_entity_count(&runtime, &token, 1).await;
        ProposalApplyWorker::new(runtime.clone())
            .maybe_apply(&token, id, &registry, None)
            .await
            .unwrap();
        assert_eq!(apply_results(&runtime, &token, id).await.len(), 1);
        assert_entity_count(&runtime, &token, 1).await;
    }
}

#[tokio::test]
async fn proposal_workspace_change_preserves_other_entities_and_add_note_admission() {
    let (runtime, token, registry) = fixture();
    let entity_id = propose_and_approve(
        &registry,
        json!({"kind": "add_entity", "entity": {"kind": "concept", "name": "ordinary"}}),
    )
    .await;
    assert!(matches!(
        &apply_results(&runtime, &token, entity_id).await[0],
        ApplyResult::Success { .. }
    ));
    let note_id = propose_and_approve(
        &registry,
        json!({
            "kind": "add_note",
            "note": {"kind": "task", "content": "approved task", "properties": {"status": "done"}},
        }),
    )
    .await;
    assert!(matches!(
        &apply_results(&runtime, &token, note_id).await[0],
        ApplyResult::Success { .. }
    ));
    let notes = registry
        .dispatch("list", json!({"kind": "task"}))
        .await
        .unwrap();
    assert_eq!(notes["items"].as_array().unwrap().len(), 1);
    assert_eq!(notes["items"][0]["properties"]["status"], "done");
    assert_entity_count(&runtime, &token, 1).await;
}

#[tokio::test]
async fn proposal_workspace_mixed_compound_remains_refused_without_domain_writes() {
    let (runtime, token, registry) = fixture();
    let error = registry.dispatch(
        "propose",
        json!({
            "title": "Mixed compound", "description": "Existing multi-step refusal",
            "changeset": {"kind": "compound", "steps": [
                {"kind": "add_entity", "entity": {"kind": "concept", "name": "must not land"}},
                {"kind": "add_entity", "entity": {"kind": "workspace", "name": "invalid", "properties": {"schema_version": "1"}}},
            ]},
        }),
    ).await.expect_err("multi-step compound admission remains unchanged");
    assert!(error.to_string().contains("multi-step Compound"), "{error}");
    assert_entity_count(&runtime, &token, 0).await;
}

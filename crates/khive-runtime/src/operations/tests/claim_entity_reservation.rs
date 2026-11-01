use crate::operations::EntityClaimSpec;
use crate::runtime::NamespaceToken;
use crate::secret_gate_finalizer::entity_admission::{
    fixture as admission_fixture, EXEMPTION_STAMP,
};
use crate::secret_gate_finalizer::manifest::{fixture::TestOnlyManifestFixture, RuntimeFieldScope};
use crate::{KhiveRuntime, Namespace, RuntimeConfig, RuntimeError};
use khive_storage::types::PageRequest;
use khive_storage::EventFilter;
use khive_types::EventKind;
use serde_json::{json, Value};
use uuid::Uuid;

const RESERVED_KEY: &str = "khive:secret_gate";

fn claim_spec(id: Uuid, name: &str, properties: Option<Value>) -> EntityClaimSpec {
    let identity_tag = format!("claim-reservation:{id}");
    EntityClaimSpec {
        id,
        kind: "concept".into(),
        entity_type: None,
        name: name.into(),
        description: None,
        properties,
        tags: vec![identity_tag.clone()],
        identity_tag,
    }
}

#[derive(Debug, PartialEq)]
struct ClaimSnapshot {
    entity: Option<Value>,
    events: Vec<Value>,
    created_events: usize,
}

async fn claim_snapshot(runtime: &KhiveRuntime, token: &NamespaceToken, id: Uuid) -> ClaimSnapshot {
    let entity = runtime
        .entities(token)
        .expect("authorized fixture entity store")
        .get_entity_including_deleted(id)
        .await
        .expect("actual stored claim readback")
        .map(|entity| serde_json::to_value(entity).expect("complete entity snapshot"));
    let page = runtime
        .events(token)
        .expect("authorized fixture event store")
        .query_events(
            EventFilter {
                target_id: Some(id),
                ..EventFilter::default()
            },
            PageRequest {
                offset: 0,
                limit: 16,
            },
        )
        .await
        .expect("actual target event readback");
    assert!(
        page.items.len() < 16,
        "the bounded fixture must retain a complete event baseline"
    );
    let created_events = page
        .items
        .iter()
        .filter(|event| event.kind == EventKind::EntityCreated)
        .count();
    let events = page
        .items
        .iter()
        .map(|event| serde_json::to_value(event).expect("complete event snapshot"))
        .collect();
    ClaimSnapshot {
        entity,
        events,
        created_events,
    }
}

#[tokio::test]
async fn claim_entity_rejects_reserved_secret_gate_key_before_insert() {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        actor_id: Some("claim-reservation-fixture".into()),
        events_split: None,
        packs: vec!["kg".into()],
        ..RuntimeConfig::no_embeddings()
    })
    .expect("explicit private-memory runtime without embedders");
    let namespace = format!("claim-reservation-{}", Uuid::new_v4());
    let token = runtime
        .authorize(Namespace::parse(&namespace).expect("unique fixture namespace"))
        .expect("real Gate-authorized fixture token");

    let clean_id = Uuid::new_v4();
    let (clean, inserted) = runtime
        .claim_entity_if_absent(
            &token,
            claim_spec(clean_id, "clean claim", Some(json!({"safe": true}))),
        )
        .await
        .expect("healthy clean claim must commit before refusal probes");
    assert!(inserted);
    assert_eq!(clean.id, clean_id);
    let clean_baseline = claim_snapshot(&runtime, &token, clean_id).await;
    let clean_row = clean_baseline
        .entity
        .as_ref()
        .expect("actual clean row exists");
    assert_eq!(clean_row["name"], "clean claim");
    assert_eq!(clean_row["properties"], json!({"safe": true}));
    assert_eq!(clean_baseline.created_events, 1);
    assert_eq!(
        clean_baseline.events.len(),
        1,
        "healthy claim has a REAL nonzero event baseline"
    );
    let (retry, inserted) = runtime
        .claim_entity_if_absent(
            &token,
            claim_spec(clean_id, "clean claim", Some(json!({"safe": true}))),
        )
        .await
        .expect("coherent clean retry must retain the winning row");
    assert!(!inserted);
    assert_eq!(retry.id, clean_id);
    assert_eq!(
        claim_snapshot(&runtime, &token, clean_id).await,
        clean_baseline
    );

    // This fixture is not evidence of operator adjudication.
    let manifest = TestOnlyManifestFixture::for_scope(RuntimeFieldScope::NameDescription);
    let _manifest_guard = admission_fixture::install(&namespace, manifest.snapshot());
    let exempt_id = Uuid::new_v4();
    let (exempt, inserted) = runtime
        .claim_entity_if_absent(&token, claim_spec(exempt_id, manifest.exact_value(), None))
        .await
        .expect("matched fixture must reach the real finalized claim path");
    assert!(inserted);
    assert_eq!(exempt.id, exempt_id);
    assert_eq!(exempt.name, manifest.exact_value());
    assert_eq!(
        exempt
            .properties
            .as_ref()
            .and_then(|properties| properties.get(RESERVED_KEY)),
        Some(&json!(EXEMPTION_STAMP))
    );
    let exempt_baseline = claim_snapshot(&runtime, &token, exempt_id).await;
    let exempt_row = exempt_baseline
        .entity
        .as_ref()
        .expect("actual finalized row exists");
    assert_eq!(exempt_row["name"], manifest.exact_value());
    assert_eq!(exempt_row["properties"][RESERVED_KEY], EXEMPTION_STAMP);
    assert_eq!(exempt_baseline.created_events, 1);
    assert_eq!(
        exempt_baseline.events.len(),
        2,
        "finalized claim retains its create and exemption events"
    );

    for (index, value) in [
        json!(EXEMPTION_STAMP),
        json!("caller-forged"),
        Value::Null,
        json!(true),
        json!(1),
        json!({}),
        json!([]),
    ]
    .into_iter()
    .enumerate()
    {
        let id = Uuid::new_v4();
        let before = claim_snapshot(&runtime, &token, id).await;
        assert!(before.entity.is_none());
        assert!(before.events.is_empty());
        let error = runtime
            .claim_entity_if_absent(
                &token,
                claim_spec(
                    id,
                    &format!("reserved claim {index}"),
                    Some(json!({"safe": "retained", "khive:secret_gate": value})),
                ),
            )
            .await
            .expect_err("caller-supplied reserved key must refuse claim");
        assert!(
            matches!(&error, RuntimeError::InvalidInput(message) if message.contains(RESERVED_KEY)),
            "expected reservation refusal, got {error:?}"
        );
        assert_eq!(claim_snapshot(&runtime, &token, id).await, before);
        assert_eq!(
            claim_snapshot(&runtime, &token, clean_id).await,
            clean_baseline
        );
        assert_eq!(
            claim_snapshot(&runtime, &token, exempt_id).await,
            exempt_baseline
        );
    }

    let echoed_properties = exempt_row
        .get("properties")
        .expect("persisted runtime stamp object")
        .clone();
    let error = runtime
        .claim_entity_if_absent(
            &token,
            claim_spec(exempt_id, manifest.exact_value(), Some(echoed_properties)),
        )
        .await
        .expect_err("a full-object caller echo remains refused on an existing claim");
    assert!(
        matches!(&error, RuntimeError::InvalidInput(message) if message.contains(RESERVED_KEY)),
        "expected echo reservation refusal, got {error:?}"
    );
    assert_eq!(
        claim_snapshot(&runtime, &token, clean_id).await,
        clean_baseline
    );
    assert_eq!(
        claim_snapshot(&runtime, &token, exempt_id).await,
        exempt_baseline
    );
}

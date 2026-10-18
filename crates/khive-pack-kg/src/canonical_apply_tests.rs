use super::*;
use std::sync::{Arc, Mutex};

use khive_changeset::Envelope;
use khive_gate::{Gate, GateDecision, GateError, GateRequest};
use khive_runtime::{RequestIdentity, VerbRegistryBuilder};
use khive_types::Timestamp;
use serde_json::{json, Value};

fn create(id: u128, name: &str) -> Value {
    json!({"op":"create","id":Uuid::from_u128(id),"namespace":"local",
        "target":{"kind":"entity","entity_kind":"concept","name":name}})
}

fn delta(ops: &[Value]) -> String {
    let mut text = serde_json::to_string(&Envelope::new(
        "producer",
        "family",
        Timestamp::from_secs(1),
    ))
    .unwrap();
    text.push('\n');
    for op in ops {
        text.push_str(&serde_json::to_string(op).unwrap());
        text.push('\n');
    }
    text
}

fn setup() -> (KhiveRuntime, VerbRegistry) {
    let runtime = KhiveRuntime::memory().unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(crate::KgPack::new(runtime.clone()));
    (runtime, builder.build().unwrap())
}

#[test]
fn canonical_delta_requires_exact_envelope_and_supported_outer_version() {
    let text = delta(&[create(1, "one"), create(2, "two")]);
    let decoded = decode_delta(&text, Some(khive_changeset::CURRENT_SCHEMA_VERSION)).unwrap();
    assert_eq!(decoded.ops.len(), 2);
    // The fixture is hand-built JSON. The byte layout belongs to the codec, so
    // the codec's own output is the fixed point a decode and re-encode keeps.
    let encoded = khive_changeset::to_ndjson(&decoded).unwrap();
    let reencoded = khive_changeset::to_ndjson(
        &decode_delta(&encoded, Some(khive_changeset::CURRENT_SCHEMA_VERSION)).unwrap(),
    )
    .unwrap();
    assert_eq!(reencoded, encoded);
    assert!(decode_delta(&text, Some(khive_changeset::CURRENT_SCHEMA_VERSION + 1)).is_err());
    assert!(decode_delta(
        &text.replacen(
            &format!(
                "\"schema_version\":{}",
                khive_changeset::CURRENT_SCHEMA_VERSION
            ),
            "\"schema_version\":1",
            1
        ),
        None
    )
    .is_err());
    assert!(decode_delta(&(text.clone() + "\n"), None).is_err());
    assert!(decode_delta(&(text + "{}\n"), None).is_err());
}

#[test]
fn canonical_bound_and_budget_count_the_whole_change_set() {
    let max = khive_types::pack::ATOMIC_MAX_OPS_DEFAULT;
    let mut ops: Vec<_> = (0..max).map(|i| create(i as u128 + 1, "bounded")).collect();
    assert_eq!(decode_delta(&delta(&ops), None).unwrap().ops.len(), max);
    ops.push(create(max as u128 + 1, "over bound"));
    assert!(decode_delta(&delta(&ops), None).is_err());
    let canonical = ProposalChangeset::Canonical {
        schema_version: khive_changeset::CURRENT_SCHEMA_VERSION,
        delta: delta(&ops[..3]),
    };
    assert_eq!(crate::apply_worker::count_new_entries(&canonical), 3);
}

#[test]
fn canonical_create_only_has_no_captured_preimage_marker() {
    assert_eq!(
        preimage_status(&decode_delta(&delta(&[create(1, "one")]), None).unwrap()),
        ProposalPreimageStatus::None
    );
    let update = json!({"op":"update","target_id":Uuid::from_u128(1),
        "patch":{"target":"entity","name":"after"},
        "preimage":{"target":"entity","name":"before"}});
    assert_eq!(
        preimage_status(&decode_delta(&delta(&[update]), None).unwrap()),
        ProposalPreimageStatus::Captured
    );
}

#[test]
fn valid_merge_is_known_unimplemented_before_any_preparation() {
    let captured = |id| {
        json!({"id":Uuid::from_u128(id),"namespace":"local",
        "created_at":1_000_000,"updated_at":1_000_000,"kind":"concept",
        "entity_type":null,"name":"captured","description":null,
        "properties":{},"tags":[],"deleted_at":null})
    };
    let merge = json!({"op":"merge","into_id":Uuid::from_u128(1),
        "from_id":Uuid::from_u128(2),"preimage":{"into":captured(1),
        "from":captured(2),"incident_edges":[]}});
    let text = delta(&[create(3, "must not be prepared"), merge]);
    assert!(khive_changeset::from_ndjson(&text).is_ok());
    let error = decode_delta(&text, None).unwrap_err();
    assert!(error.to_string().contains("KnownUnimplemented"));
}

#[test]
fn canonical_note_tags_alias_refuses_while_entity_custom_tags_survives() {
    let (_runtime, registry) = setup();
    let note = json!({"op":"create","id":Uuid::from_u128(1),"namespace":"local",
        "target":{"kind":"note","note_kind":"observation","content":"note",
        "tags":["top"],"properties":{"tags":["aliased"]}}});
    let decoded = decode_delta(&delta(&[note]), None).unwrap();
    assert!(validate_canonical_admission(&decoded, &registry).is_err());
    let mut entity = create(1, "ordinary custom property");
    entity["target"]["properties"] = json!({"tags":["ordered","property"]});
    assert!(validate_canonical_admission(
        &decode_delta(&delta(&[entity]), None).unwrap(),
        &registry
    )
    .is_ok());
}

#[tokio::test]
async fn canonical_dispatch_preserves_staged_ids_and_collision_rolls_back_earlier_create() {
    let (runtime, registry) = setup();
    let id = Uuid::from_u128(101);
    let result = registry
        .dispatch(
            "apply_changeset",
            json!({"delta":delta(&[create(101,"stable")]),"namespace":"local"}),
        )
        .await
        .unwrap();
    assert_eq!(result["committed"], true);
    assert_eq!(result["preimage"], "none");
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .unwrap();
    assert_eq!(runtime.get_entity(&token, id).await.unwrap().id, id);
    let repeated = delta(&[create(102, "must roll back"), create(101, "collision")]);
    assert!(registry
        .dispatch("apply_changeset", json!({"delta":repeated}))
        .await
        .is_err());
    assert!(runtime
        .entities(&token)
        .unwrap()
        .get_entity(Uuid::from_u128(102))
        .await
        .unwrap()
        .is_none());
    assert_eq!(runtime.get_entity(&token, id).await.unwrap().name, "stable");
}

#[tokio::test]
async fn canonical_dispatch_cannot_accept_client_limits_or_live_batch_conversion() {
    let (_runtime, registry) = setup();
    for extra in ["max_ops", "items", "ops", "atomic"] {
        let mut args = json!({"delta":delta(&[create(1,"strict")])});
        args[extra] = json!(1);
        assert!(
            registry.dispatch("apply_changeset", args).await.is_err(),
            "{extra}"
        );
    }
}

#[tokio::test]
async fn strict_secret_refusal_reports_index_and_detector_without_partial_create() {
    let (runtime, registry) = setup();
    let rejected = format!(
        "postgresql://{}:{}@fixture.example/database",
        "fixture-user", "<PASSWORD>"
    );
    let text = delta(&[create(601, "must roll back"), create(602, &rejected)]);
    let error = registry
        .dispatch("apply_changeset", json!({"delta":text}))
        .await
        .unwrap_err();
    assert!(matches!(
        error.refusal_source(),
        RuntimeError::SecretDetected(_)
    ));
    let wire = khive_runtime::runtime_error_value(error, khive_runtime::DomainDisposition::Unknown);
    assert_eq!(wire["op_index"], 1);
    assert_eq!(wire["code"], "secret_detected");
    assert_eq!(wire["detector"], "url-userinfo");
    assert_eq!(wire["domain_disposition"], "not_committed");
    assert!(!wire.to_string().contains(&rejected));
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .unwrap();
    for id in [601, 602] {
        assert!(runtime
            .entities(&token)
            .unwrap()
            .get_entity(Uuid::from_u128(id))
            .await
            .unwrap()
            .is_none());
    }
}

#[tokio::test]
async fn canonical_proposal_and_direct_apply_share_captured_update_execution() {
    let (runtime, registry) = setup();
    registry
        .dispatch(
            "apply_changeset",
            json!({"delta":delta(&[create(301,"before")])}),
        )
        .await
        .unwrap();
    let update = json!({"op":"update","target_id":Uuid::from_u128(301),
        "patch":{"target":"entity","name":"after"},
        "preimage":{"target":"entity","name":"before"}});
    let proposal = registry.dispatch("propose", json!({
        "title":"Canonical captured update", "description":"Apply the staged complete delta",
        "changeset":{"kind":"canonical","schema_version":khive_changeset::CURRENT_SCHEMA_VERSION,"delta":delta(&[update])},
    })).await.unwrap();
    let response = registry
        .dispatch(
            "review",
            json!({
                "id":proposal["id"],"decision":"approve",
            }),
        )
        .await
        .unwrap();
    assert_eq!(response["preimage"], "captured");
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .unwrap();
    assert_eq!(
        runtime
            .get_entity(&token, Uuid::from_u128(301))
            .await
            .unwrap()
            .name,
        "after"
    );
}

#[tokio::test]
async fn canonical_proposal_counts_all_creates_before_apply_budget() {
    let (runtime, registry) = setup();
    let proposal = registry.dispatch("propose", json!({
        "title":"Bounded canonical creates", "description":"Both create rows count against the budget",
        "changeset":{"kind":"canonical","schema_version":khive_changeset::CURRENT_SCHEMA_VERSION,
            "delta":delta(&[create(401,"one"),create(402,"two")])},
    })).await.unwrap();
    registry
        .dispatch(
            "review",
            json!({
                "id":proposal["id"],"decision":"approve","max_new_entries":1,
            }),
        )
        .await
        .unwrap();
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .unwrap();
    for id in [401, 402] {
        assert!(runtime
            .entities(&token)
            .unwrap()
            .get_entity(Uuid::from_u128(id))
            .await
            .unwrap()
            .is_none());
    }
}

#[tokio::test]
async fn canonical_proposal_scans_decoded_delta_values_for_secrets() {
    let (runtime, registry) = setup();
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .unwrap();
    let events = runtime.events(&token).unwrap();
    let filter = khive_storage::EventFilter {
        kinds: vec![khive_types::EventKind::ProposalCreated],
        ..Default::default()
    };
    let before = events.count_events(filter.clone()).await.unwrap();
    let rejected = format!(
        "postgresql://{}:{}@fixture.example/database",
        "fixture-user", "<PASSWORD>"
    );
    let plain = delta(&[create(701, &rejected)]);
    // A JSON escape for `@` keeps the credential's shape out of the NDJSON text; decoding the
    // delta restores it.
    let escaped = plain.replacen("@fixture.example", "\\u0040fixture.example", 1);
    assert_ne!(escaped, plain);
    khive_runtime::secret_gate::check_at(&escaped, "proposal", "changeset")
        .expect("the undecoded text passes a scan of the text alone");
    let error = registry
        .dispatch(
            "propose",
            json!({
                "title":"Escaped credential", "description":"Refuse on the decoded value",
                "changeset":{"kind":"canonical","schema_version":khive_changeset::CURRENT_SCHEMA_VERSION,"delta":escaped},
            }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error.refusal_source(), RuntimeError::SecretDetected(_)),
        "{error:?}"
    );
    assert_eq!(events.count_events(filter).await.unwrap(), before);
}

#[tokio::test]
async fn canonical_proposal_scans_values_under_reserved_property_keys() {
    let (runtime, registry) = setup();
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .unwrap();
    let events = runtime.events(&token).unwrap();
    let filter = khive_storage::EventFilter {
        kinds: vec![khive_types::EventKind::ProposalCreated],
        ..Default::default()
    };
    let before = events.count_events(filter.clone()).await.unwrap();
    // A reserved key does not exempt its value: the proposal stores the delta before apply
    // decides whether the key may be written.
    let rejected = format!(
        "postgresql://{}:{}@fixture.example/database",
        "fixture-user", "<PASSWORD>"
    );
    let update = json!({"op":"update","target_id":Uuid::from_u128(801),
        "patch":{"target":"entity","properties":{"khive:secret_gate":rejected}},
        "preimage":{"target":"entity","properties":{"khive:secret_gate":rejected}}});
    let plain = delta(&[update]);
    // JSON escapes keep the credential's shape and the key's trigger word out of the NDJSON
    // text, so only a scan of the decoded values can refuse it.
    let escaped = plain
        .replace("@fixture.example", "\\u0040fixture.example")
        .replace("khive:secret_gate", "khive:\\u0073ecret_gate");
    assert_ne!(escaped, plain);
    assert!(!escaped.contains("secret"));
    khive_runtime::secret_gate::check_at(&escaped, "proposal", "changeset")
        .expect("the undecoded text passes a scan of the text alone");
    let error = registry
        .dispatch(
            "propose",
            json!({
                "title":"Reserved credential", "description":"Refuse on the decoded value",
                "changeset":{"kind":"canonical","schema_version":khive_changeset::CURRENT_SCHEMA_VERSION,"delta":escaped},
            }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error.refusal_source(), RuntimeError::SecretDetected(_)),
        "{error:?}"
    );
    assert_eq!(events.count_events(filter).await.unwrap(), before);
}

/// The value the secret gate refuses once decoded; the tests below hide it with a JSON
/// escape so the undecoded text passes a scan of the text alone.
fn escaped_credential_fixture() -> String {
    format!(
        "postgresql://{}:{}@fixture.example/database",
        "fixture-user", "<PASSWORD>"
    )
}

/// Proposes `delta` as a canonical change-set and reports the result beside how many
/// ProposalCreated events existed before and after.
async fn propose_canonical(
    runtime: &KhiveRuntime,
    registry: &VerbRegistry,
    delta: String,
) -> (Result<Value, RuntimeError>, u64, u64) {
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .unwrap();
    let events = runtime.events(&token).unwrap();
    let filter = khive_storage::EventFilter {
        kinds: vec![khive_types::EventKind::ProposalCreated],
        ..Default::default()
    };
    let before = events.count_events(filter.clone()).await.unwrap();
    let result = registry
        .dispatch(
            "propose",
            json!({
                "title":"Canonical proposal", "description":"Decoded once, scanned whole",
                "changeset":{"kind":"canonical","schema_version":khive_changeset::CURRENT_SCHEMA_VERSION,"delta":delta},
            }),
        )
        .await;
    let after = events.count_events(filter).await.unwrap();
    (result, before, after)
}

fn assert_repeated_member_refusal(result: &Result<Value, RuntimeError>) {
    let refusal = result.as_ref().err().map(RuntimeError::refusal_source);
    assert!(
        matches!(
            refusal,
            Some(RuntimeError::InvalidInput(message)) if message.contains("repeats a member name")
        ),
        "{result:?}"
    );
}

#[tokio::test]
async fn canonical_proposal_refuses_a_repeated_property_that_hides_an_escaped_credential() {
    let (runtime, registry) = setup();
    let op = json!({"op":"create","id":Uuid::from_u128(901),"namespace":"local",
        "target":{"kind":"entity","entity_kind":"concept","name":"repeated",
            "properties":{"source":escaped_credential_fixture(),"zz_repeat":"benign"}}});
    // A decode keeps the last copy of a repeated name, so the escaped credential in the
    // first copy would be stored with the delta while every decoded scan saw "benign".
    let repeated = delta(&[op])
        .replacen("\"zz_repeat\"", "\"source\"", 1)
        .replacen("@fixture.example", "\\u0040fixture.example", 1);
    assert_eq!(repeated.matches("\"source\"").count(), 2, "{repeated}");
    khive_runtime::secret_gate::check_at(&repeated, "proposal", "changeset")
        .expect("the undecoded text passes a scan of the text alone");
    let (result, before, after) = propose_canonical(&runtime, &registry, repeated).await;
    assert_repeated_member_refusal(&result);
    assert_eq!(
        after, before,
        "a refused proposal writes no ProposalCreated"
    );
}

#[tokio::test]
async fn canonical_proposal_refuses_a_repeated_property_that_hides_an_escaped_token() {
    let (runtime, registry) = setup();
    let op = json!({"op":"create","id":Uuid::from_u128(905),"namespace":"local",
        "target":{"kind":"entity","entity_kind":"concept","name":"repeated",
            "properties":{"k":format!("ghp_{}", "A".repeat(36)),"zz_repeat":"safe"}}});
    // `g` decodes to `g`, so the prefix the detector keys on appears only once decoded.
    let repeated =
        delta(&[op])
            .replacen("\"zz_repeat\"", "\"k\"", 1)
            .replacen("\"ghp_", "\"\\u0067hp_", 1);
    assert_eq!(repeated.matches("\"k\"").count(), 2, "{repeated}");
    assert!(!repeated.contains("ghp_"), "{repeated}");
    khive_runtime::secret_gate::check_at(&repeated, "proposal", "changeset")
        .expect("the undecoded text passes a scan of the text alone");
    let (result, before, after) = propose_canonical(&runtime, &registry, repeated).await;
    assert_repeated_member_refusal(&result);
    assert_eq!(
        after, before,
        "a refused proposal writes no ProposalCreated"
    );
}

#[tokio::test]
async fn canonical_proposal_refuses_a_repeated_name_spelled_with_an_escape() {
    let (runtime, registry) = setup();
    let op = json!({"op":"create","id":Uuid::from_u128(903),"namespace":"local",
        "target":{"kind":"entity","entity_kind":"concept","name":"respelled",
            "properties":{"source":escaped_credential_fixture(),"zz_repeat":"benign"}}});
    // `\u0073ource` decodes to `source`: two spellings, one member name.
    let respelled = delta(&[op])
        .replacen("\"zz_repeat\"", "\"\\u0073ource\"", 1)
        .replacen("@fixture.example", "\\u0040fixture.example", 1);
    assert_eq!(respelled.matches("\"source\"").count(), 1, "{respelled}");
    khive_runtime::secret_gate::check_at(&respelled, "proposal", "changeset")
        .expect("the undecoded text passes a scan of the text alone");
    let (result, before, after) = propose_canonical(&runtime, &registry, respelled).await;
    assert_repeated_member_refusal(&result);
    assert_eq!(
        after, before,
        "a refused proposal writes no ProposalCreated"
    );
}

#[tokio::test]
async fn canonical_proposal_refuses_a_repeated_reserved_property_key() {
    let (runtime, registry) = setup();
    let stamp = "exempted:content-sha256-manifest-v1";
    let update = json!({"op":"update","target_id":Uuid::from_u128(902),
        "patch":{"target":"entity",
            "properties":{"khive:secret_gate":escaped_credential_fixture(),"zz_repeat":stamp}},
        "preimage":{"target":"entity","properties":{"khive:secret_gate":stamp}}});
    // With the last copy kept, the patch would decode to the runtime's own stamp, which
    // the proposal scan admits, while the stored delta held the credential.
    let repeated = delta(&[update])
        .replacen("\"zz_repeat\"", "\"khive:secret_gate\"", 1)
        .replacen("@fixture.example", "\\u0040fixture.example", 1)
        .replace("khive:secret_gate", "khive:\\u0073ecret_gate");
    assert_eq!(
        repeated.matches("khive:\\u0073ecret_gate").count(),
        3,
        "{repeated}"
    );
    assert!(!repeated.contains("secret"), "{repeated}");
    khive_runtime::secret_gate::check_at(&repeated, "proposal", "changeset")
        .expect("the undecoded text passes a scan of the text alone");
    let (result, before, after) = propose_canonical(&runtime, &registry, repeated).await;
    assert_repeated_member_refusal(&result);
    assert_eq!(
        after, before,
        "a refused proposal writes no ProposalCreated"
    );
}

#[tokio::test]
async fn canonical_proposal_with_distinct_property_names_is_admitted() {
    let (runtime, registry) = setup();
    let op = json!({"op":"create","id":Uuid::from_u128(904),"namespace":"local",
        "target":{"kind":"entity","entity_kind":"concept","name":"distinct",
            "properties":{"source":"benign","zz_repeat":"benign"}}});
    let (result, before, after) = propose_canonical(&runtime, &registry, delta(&[op])).await;
    result.expect("distinct property names are admitted");
    assert_eq!(
        after,
        before + 1,
        "an admitted proposal writes one ProposalCreated"
    );
}

#[tokio::test]
async fn canonical_proposal_admits_the_runtime_secret_gate_stamp() {
    let (_runtime, registry) = setup();
    // The value the runtime itself writes under the reserved key; an operation that carries it
    // back as a preimage echo passes the proposal scan, and apply decides the echo.
    let stamp = "exempted:content-sha256-manifest-v1";
    let update = json!({"op":"update","target_id":Uuid::from_u128(802),
        "patch":{"target":"entity","properties":{"khive:secret_gate":stamp}},
        "preimage":{"target":"entity","properties":{"khive:secret_gate":stamp}}});
    registry
        .dispatch(
            "propose",
            json!({
                "title":"Stamp echo", "description":"Apply decides reserved values",
                "changeset":{"kind":"canonical","schema_version":khive_changeset::CURRENT_SCHEMA_VERSION,"delta":delta(&[update])},
            }),
        )
        .await
        .expect("the runtime's own stamp passes the proposal scan");
}

#[tokio::test]
async fn invalid_canonical_proposal_leaves_no_created_history() {
    let (runtime, registry) = setup();
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .unwrap();
    let events = runtime.events(&token).unwrap();
    let filter = khive_storage::EventFilter {
        kinds: vec![khive_types::EventKind::ProposalCreated],
        ..Default::default()
    };
    let before = events.count_events(filter.clone()).await.unwrap();
    for changeset in [
        json!({"kind":"canonical","schema_version":khive_changeset::CURRENT_SCHEMA_VERSION + 1,"delta":delta(&[create(501,"invalid")])}),
        json!({"kind":"compound","steps":[{"kind":"canonical","schema_version":khive_changeset::CURRENT_SCHEMA_VERSION,
            "delta":delta(&[create(502,"nested")])}]}),
    ] {
        assert!(registry
            .dispatch(
                "propose",
                json!({
                    "title":"Invalid canonical", "description":"Refuse before proposal history",
                    "changeset":changeset,
                })
            )
            .await
            .is_err());
    }
    assert_eq!(events.count_events(filter).await.unwrap(), before);
}

#[derive(Debug)]
struct ConstituentGate {
    seen: Mutex<Vec<GateRequest>>,
}

impl Gate for ConstituentGate {
    fn check(&self, request: &GateRequest) -> Result<GateDecision, GateError> {
        self.seen.lock().unwrap().push(request.clone());
        if request.verb == "create" && request.args.get("name") == Some(&json!("denied")) {
            Ok(GateDecision::deny("fixture constituent denial"))
        } else {
            Ok(GateDecision::allow())
        }
    }
}

#[tokio::test]
async fn canonical_constituent_denial_keeps_real_caller_and_prevents_all_graph_mutation() {
    let runtime = KhiveRuntime::memory().unwrap();
    let gate = Arc::new(ConstituentGate {
        seen: Mutex::new(Vec::new()),
    });
    let mut builder = VerbRegistryBuilder::new();
    builder.with_gate(gate.clone());
    builder.register(crate::KgPack::new(runtime.clone()));
    let registry = builder.build().unwrap();
    let text = delta(&[create(201, "allowed"), create(202, "denied")]);
    let identity = RequestIdentity {
        namespace: "local".into(),
        actor_id: Some("lambda:canonical-test".into()),
        visible_namespaces: vec![],
        process_ref: Some("fixture-process".into()),
        request_id: Some(91),
    };
    assert!(registry
        .dispatch_with_identity("apply_changeset", json!({"delta":text}), Some(identity))
        .await
        .is_err());
    // A snapshot, so no lock is held across the reads below that await.
    let seen = gate.seen.lock().unwrap().clone();
    // Dispatch gates the wrapper once; the landing path does not check it again.
    assert_eq!(
        seen.iter()
            .filter(|request| request.verb == "apply_changeset")
            .count(),
        1
    );
    assert!(seen
        .iter()
        .any(|request| request.verb == "create" && request.args["name"] == "denied"));
    assert!(seen
        .iter()
        .all(|request| request.actor.binding_id() == Some("lambda:canonical-test")));
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .unwrap();
    for id in [201, 202] {
        assert!(runtime
            .entities(&token)
            .unwrap()
            .get_entity(Uuid::from_u128(id))
            .await
            .unwrap()
            .is_none());
    }
}

#[tokio::test]
async fn canonical_restore_dispatch_refuses_before_creates_or_events() {
    let (runtime, registry) = setup();
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .unwrap();
    let events = runtime.events(&token).unwrap();
    let before = events.count_events(Default::default()).await.unwrap();
    let restore = json!({"op":"restore","target_id":Uuid::from_u128(802),
        "record":{"substrate":"entity","id":Uuid::from_u128(802),"namespace":"local",
            "created_at":1_000_000,"updated_at":1_000_000,"kind":"concept",
            "entity_type":null,"name":"restored","description":null,
            "properties":{},"tags":[],"deleted_at":null}});
    let text = delta(&[create(801, "before"), restore, create(803, "after")]);
    assert_eq!(decode_delta(&text, None).unwrap().ops.len(), 3);
    let error = registry
        .dispatch("apply_changeset", json!({"delta":text}))
        .await
        .unwrap_err();
    assert!(matches!(
        error.refusal_source(),
        RuntimeError::CanonicalUnsupportedOperation {
            op_index: 1,
            operation: "restore",
        }
    ));
    let wire = khive_runtime::runtime_error_value(error, khive_runtime::DomainDisposition::Unknown);
    assert_eq!(wire["op_index"], 1);
    assert_eq!(wire["operation"], "restore");
    assert_eq!(wire["reason"], "KnownUnimplemented");
    assert_eq!(wire["domain_disposition"], "not_committed");
    for id in [801, 802, 803] {
        assert!(runtime
            .entities(&token)
            .unwrap()
            .get_entity(Uuid::from_u128(id))
            .await
            .unwrap()
            .is_none());
    }
    assert_eq!(
        events.count_events(Default::default()).await.unwrap(),
        before
    );
}

use super::*;
use khive_gate::ActorRef;
use serde_json::json;

fn request(verb: &str) -> GateRequest {
    GateRequest::new(
        ActorRef::new("actor", "truncate-audit-test"),
        Namespace::parse("local").unwrap(),
        verb,
        json!({"stream": "s", "before_seq": i64::MAX}),
    )
}

#[test]
fn truncation_enrichment_preserves_plain_and_singleton_link_payload_bytes() {
    for verb in ["stream.stat", "link"] {
        let req = request(verb);
        let audit = masked_audit_event(&req, &GateDecision::allow(), "test");
        let resource = json!({"work_class": "interactive", "cost_unit": 1});
        // These are the unchanged main-branch builders, including link's v2
        // result enrichment. Compare serialized bytes, not a subset of fields.
        let mut event =
            build_audit_storage_event(&req, &audit, EventOutcome::Success, Some(resource.clone()));
        if verb == "link" {
            let result = json!({
                "id": "00000000-0000-0000-0000-000000000001",
                "source_id": "00000000-0000-0000-0000-000000000002",
                "target_id": "00000000-0000-0000-0000-000000000003",
                "relation": "relates_to", "weight": 0.75,
            });
            let (_, mut payload) = link_audit_success_from_result(audit, &result).unwrap();
            payload["resource"] = resource;
            event = event.with_payload(payload).with_payload_schema_version(2);
        }
        let before = serde_json::to_vec(&event.payload).unwrap();
        enrich_stream_truncate_success(&mut event, &req, &json!({"removed": 3}));
        assert_eq!(
            serde_json::to_vec(&event.payload).unwrap(),
            before,
            "{verb}"
        );
    }
}

#[test]
fn truncation_enrichment_adds_only_original_bound_and_result_count() {
    let req = request("stream.truncate");
    let audit = masked_audit_event(&req, &GateDecision::allow(), "test");
    let mut event = build_audit_storage_event(
        &req,
        &audit,
        EventOutcome::Success,
        Some(json!({"work_class": "interactive", "cost_unit": 1})),
    );
    let original = event.payload.clone();
    enrich_stream_truncate_success(
        &mut event,
        &req,
        &json!({"removed": 3, "floor_seq": 4, "head_seq": 3}),
    );
    let payload = event.payload.as_object_mut().unwrap();
    assert_eq!(payload.remove("stream"), Some(json!("s")));
    assert_eq!(payload.remove("before_seq"), Some(json!(i64::MAX)));
    assert_eq!(payload.remove("removed"), Some(json!(3)));
    assert_eq!(event.payload, original);
}

use std::sync::Arc;

use serde_json::json;

use crate::{
    ActorRef, AllowAllGate, AuditDecision, AuditEvent, Gate, GateContext, GateDecision, GateError,
    GateRef, GateRequest, GateValidationError, Obligation,
};
use khive_types::Namespace;

fn sample_request() -> GateRequest {
    GateRequest::new(
        ActorRef::anonymous(),
        Namespace::local(),
        "search",
        json!({"query": "LoRA"}),
    )
}

#[test]
fn allow_all_gate_allows() {
    let gate = AllowAllGate;
    let decision = gate.check(&sample_request()).unwrap();
    assert!(decision.is_allow());
}

#[test]
fn allow_all_gate_through_dyn() {
    let gate: GateRef = Arc::new(AllowAllGate);
    let decision = gate.check(&sample_request()).unwrap();
    assert!(decision.is_allow());
}

#[test]
fn actor_ref_anonymous() {
    let a = ActorRef::anonymous();
    assert_eq!(a.kind, "anonymous");
    assert_eq!(a.id, "local");
}

#[test]
fn decision_helpers() {
    assert!(GateDecision::allow().is_allow());
    assert!(!GateDecision::deny("nope").is_allow());
}

#[test]
fn request_serializes_to_stable_shape() {
    let req = sample_request();
    let v = serde_json::to_value(&req).unwrap();
    assert_eq!(v["actor"]["kind"], "anonymous");
    assert_eq!(v["actor"]["id"], "local");
    assert_eq!(v["namespace"], "local");
    assert_eq!(v["verb"], "search");
    assert_eq!(v["args"]["query"], "LoRA");
}

#[test]
fn decision_roundtrips_through_json() {
    let allow = GateDecision::allow_with(vec![Obligation::Audit {
        tag: "search.attempt".into(),
    }]);
    let s = serde_json::to_string(&allow).unwrap();
    let back: GateDecision = serde_json::from_str(&s).unwrap();
    match back {
        GateDecision::Allow { obligations } => {
            assert_eq!(obligations.len(), 1);
            match &obligations[0] {
                Obligation::Audit { tag } => assert_eq!(tag, "search.attempt"),
                _ => panic!("expected Audit"),
            }
        }
        _ => panic!("expected Allow"),
    }

    let deny = GateDecision::deny("forbidden");
    let s = serde_json::to_string(&deny).unwrap();
    let back: GateDecision = serde_json::from_str(&s).unwrap();
    match back {
        GateDecision::Deny { reason } => assert_eq!(reason, "forbidden"),
        _ => panic!("expected Deny"),
    }
}

#[test]
fn obligation_rate_limit_serializes_with_kind_tag() {
    let o = Obligation::RateLimit {
        window_secs: 60,
        max: 100,
    };
    let v = serde_json::to_value(&o).unwrap();
    assert_eq!(v["kind"], "rate_limit");
    assert_eq!(v["window_secs"], 60);
    assert_eq!(v["max"], 100);
}

// `Obligation::Custom` must carry arbitrary JSON. The struct-like variant shape
// is mandatory here because an internally-tagged newtype variant cannot merge
// the `kind` discriminator into a non-object payload — a previous newtype shape
// failed for scalar/array values at runtime instead of compile time, exactly
// the foot-gun this guards.
fn assert_custom_round_trips(value: serde_json::Value) {
    let original = Obligation::Custom {
        value: value.clone(),
    };
    let json = serde_json::to_value(&original).expect("serialize");
    assert_eq!(json["kind"], "custom");
    assert_eq!(json["value"], value);
    let back: Obligation = serde_json::from_value(json).expect("deserialize");
    match back {
        Obligation::Custom { value: got } => assert_eq!(got, value),
        other => panic!("expected Custom, got {other:?}"),
    }
}

#[test]
fn obligation_custom_round_trips_object() {
    assert_custom_round_trips(serde_json::json!({"audit_tag": "billing", "weight": 1.5}));
}

#[test]
fn obligation_custom_round_trips_string() {
    assert_custom_round_trips(serde_json::json!("just a string"));
}

#[test]
fn obligation_custom_round_trips_number() {
    assert_custom_round_trips(serde_json::json!(42));
}

#[test]
fn obligation_custom_round_trips_array() {
    assert_custom_round_trips(serde_json::json!(["a", "b", 3]));
}

#[test]
fn obligation_custom_round_trips_null() {
    assert_custom_round_trips(serde_json::Value::Null);
}

#[test]
fn obligation_custom_round_trips_bool() {
    assert_custom_round_trips(serde_json::json!(true));
}

// ---- AuditEvent ----

fn sample_req_with_session() -> GateRequest {
    GateRequest::new(
        ActorRef::new("user", "operator"),
        Namespace::local(),
        "create",
        json!({"kind": "concept"}),
    )
    .with_context(GateContext {
        session_id: Some("sess-abc".into()),
        timestamp: None,
        source: Some("mcp".into()),
    })
}

#[test]
fn audit_event_roundtrips_through_serde_stable_shape() {
    let req = sample_req_with_session();
    let decision = GateDecision::allow_with(vec![Obligation::Audit {
        tag: "create.attempt".into(),
    }]);
    let ev = AuditEvent::from_check(&req, &decision, "AllowAllGate");

    let json = serde_json::to_value(&ev).unwrap();

    // All required fields present with correct values.
    assert_eq!(json["actor"]["kind"], "user");
    assert_eq!(json["actor"]["id"], "operator");
    assert_eq!(json["namespace"], "local");
    assert_eq!(json["verb"], "create");
    assert_eq!(json["decision"], "allow");
    assert_eq!(json["gate_impl"], "AllowAllGate");
    assert_eq!(json["session_id"], "sess-abc");
    // deny_reason absent on Allow.
    assert!(json.get("deny_reason").is_none() || json["deny_reason"].is_null());
    // obligations populated.
    assert_eq!(json["obligations"][0]["kind"], "audit");
    assert_eq!(json["obligations"][0]["tag"], "create.attempt");
    // timestamp present and non-null.
    assert!(json["timestamp"].is_string());

    // Full round-trip.
    let back: AuditEvent = serde_json::from_value(json).unwrap();
    assert_eq!(back.verb, "create");
    assert_eq!(back.decision, AuditDecision::Allow);
    assert!(back.deny_reason.is_none());
    assert_eq!(back.obligations.len(), 1);
}

#[test]
fn audit_event_deny_path_carries_reason() {
    let req = sample_request(); // anonymous, no session
    let decision = GateDecision::deny("forbidden: no write for anonymous");
    let ev = AuditEvent::from_check(&req, &decision, "RegoGate");

    let json = serde_json::to_value(&ev).unwrap();

    assert_eq!(json["decision"], "deny");
    assert_eq!(json["deny_reason"], "forbidden: no write for anonymous");
    assert_eq!(json["gate_impl"], "RegoGate");
    // obligations is always present on the wire, empty on Deny.
    assert_eq!(
        json["obligations"],
        serde_json::Value::Array(Vec::new()),
        "obligations must be an empty array on Deny, not omitted"
    );
    // session_id absent when not in context.
    assert!(json.get("session_id").is_none() || json["session_id"].is_null());
}

#[test]
fn audit_event_allow_no_obligations() {
    let req = sample_request();
    let decision = GateDecision::allow();
    let ev = AuditEvent::from_check(&req, &decision, "AllowAllGate");
    assert_eq!(ev.decision, AuditDecision::Allow);
    assert!(ev.deny_reason.is_none());
    assert!(ev.obligations.is_empty());
    // obligations is always present on the wire as an empty array — the
    // public JSON contract does not depend on Rust's `#[serde(default)]`
    // behavior at the consumer side.
    let json = serde_json::to_value(&ev).unwrap();
    assert_eq!(
        json["obligations"],
        serde_json::Value::Array(Vec::new()),
        "obligations must serialize as an empty array, not be omitted"
    );
}

#[test]
fn audit_decision_serialises_as_snake_case() {
    let allow = serde_json::to_value(AuditDecision::Allow).unwrap();
    assert_eq!(allow, "allow");
    let deny = serde_json::to_value(AuditDecision::Deny).unwrap();
    assert_eq!(deny, "deny");
}

// ---- GATE-AUD-001: impl_name() default ----

// A gate that does NOT override impl_name() — default must return type_name.
#[derive(Debug)]
struct CustomTestGate;

impl Gate for CustomTestGate {
    fn check(&self, _req: &GateRequest) -> Result<GateDecision, GateError> {
        Ok(GateDecision::allow())
    }
    // impl_name intentionally NOT overridden — tests the default.
}

#[test]
fn impl_name_default_returns_type_name() {
    let gate = CustomTestGate;
    // The default must use std::any::type_name, not the literal "Gate".
    assert_ne!(
        gate.impl_name(),
        "Gate",
        "default impl_name must not return literal \"Gate\""
    );
    assert!(
        gate.impl_name().contains("CustomTestGate"),
        "default impl_name must contain the concrete type name, got: {}",
        gate.impl_name()
    );
}

#[test]
fn allow_all_gate_impl_name_is_overridden() {
    let gate = AllowAllGate;
    assert_eq!(gate.impl_name(), "AllowAllGate");
}

// ---- GATE-AUD-002: validation rejection at deserialization boundary ----

#[test]
fn deserialize_rejects_empty_actor_kind() {
    let json = r#"{"kind":"","id":"x"}"#;
    let err = serde_json::from_str::<ActorRef>(json).unwrap_err();
    assert!(err.to_string().contains("actor kind must not be empty"));
}

#[test]
fn deserialize_rejects_empty_actor_id() {
    let json = r#"{"kind":"user","id":""}"#;
    let err = serde_json::from_str::<ActorRef>(json).unwrap_err();
    assert!(err.to_string().contains("actor id must not be empty"));
}

#[test]
fn deserialize_rejects_empty_verb() {
    let json = r#"{"actor":{"kind":"user","id":"x"},"namespace":"local","verb":"","args":{}}"#;
    let err = serde_json::from_str::<GateRequest>(json).unwrap_err();
    assert!(err.to_string().contains("verb must not be empty"));
}

#[test]
fn deserialize_rejects_empty_deny_reason() {
    let json = r#"{"decision":"deny","reason":""}"#;
    let err = serde_json::from_str::<GateDecision>(json).unwrap_err();
    assert!(err.to_string().contains("deny reason must not be empty"));
}

// ---- GATE-006: Obligation::Audit rejects empty tag ----

#[test]
fn deserialize_rejects_empty_audit_tag() {
    let json = r#"{"kind":"audit","tag":""}"#;
    let err = serde_json::from_str::<Obligation>(json).unwrap_err();
    assert!(
        err.to_string().contains("audit tag must not be empty"),
        "wrong error: {err}"
    );
}

#[test]
fn deserialize_accepts_nonempty_audit_tag() {
    let json = r#"{"kind":"audit","tag":"verb.search"}"#;
    let obligation = serde_json::from_str::<Obligation>(json).unwrap();
    match obligation {
        Obligation::Audit { tag } => assert_eq!(tag, "verb.search"),
        other => panic!("expected Audit, got {other:?}"),
    }
}

#[test]
fn deserialize_rejects_zero_rate_limit_window() {
    let json = r#"{"kind":"rate_limit","window_secs":0,"max":10}"#;
    let err = serde_json::from_str::<Obligation>(json).unwrap_err();
    assert!(err
        .to_string()
        .contains("rate limit window_secs must be > 0"));
}

#[test]
fn deserialize_rejects_zero_rate_limit_max() {
    let json = r#"{"kind":"rate_limit","window_secs":60,"max":0}"#;
    let err = serde_json::from_str::<Obligation>(json).unwrap_err();
    assert!(err.to_string().contains("rate limit max must be > 0"));
}

#[test]
fn gate_decision_unknown_kind_rejects() {
    let json = r#"{"decision":"maybe","reason":"nope"}"#;
    assert!(
        serde_json::from_str::<GateDecision>(json).is_err(),
        "unknown decision tag must be rejected"
    );
}

#[test]
fn obligation_unknown_kind_rejects() {
    let json = r#"{"kind":"unknown_obligation","value":1}"#;
    assert!(
        serde_json::from_str::<Obligation>(json).is_err(),
        "unknown obligation kind must be rejected"
    );
}

// ---- try_new constructor validation ----

#[test]
fn actor_ref_try_new_rejects_empty_kind() {
    assert_eq!(
        ActorRef::try_new("", "id"),
        Err(GateValidationError::EmptyActorKind)
    );
}

#[test]
fn actor_ref_try_new_rejects_empty_id() {
    assert_eq!(
        ActorRef::try_new("user", ""),
        Err(GateValidationError::EmptyActorId)
    );
}

#[test]
fn gate_request_try_new_rejects_empty_verb() {
    let err =
        GateRequest::try_new(ActorRef::anonymous(), Namespace::local(), "", json!({})).unwrap_err();
    assert_eq!(err, GateValidationError::EmptyVerb);
}

#[test]
fn deny_try_deny_rejects_empty_reason() {
    let err = GateDecision::try_deny("").unwrap_err();
    assert_eq!(err, GateValidationError::EmptyDenyReason);
}

#[test]
fn rate_limit_try_rejects_zero_window() {
    let err = Obligation::try_rate_limit(0, 10).unwrap_err();
    assert_eq!(err, GateValidationError::ZeroRateLimitWindow);
}

#[test]
fn rate_limit_try_rejects_zero_max() {
    let err = Obligation::try_rate_limit(60, 0).unwrap_err();
    assert_eq!(err, GateValidationError::ZeroRateLimitMax);
}

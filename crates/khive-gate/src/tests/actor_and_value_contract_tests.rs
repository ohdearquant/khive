#[test]
fn actor_ref_anonymous() {
    let a = ActorRef::anonymous();
    assert_eq!(a.kind, "anonymous");
    assert_eq!(a.id, "local");
}

#[test]
fn anonymous_actor_binding_id_is_none() {
    let a = ActorRef::anonymous();
    assert!(a.is_anonymous());
    assert_eq!(
        a.binding_id(),
        None,
        "anonymous actor must never expose a binding_id, even though its \
         raw id (\"local\") could otherwise match an explicit actor=\"local\" binding"
    );
}

#[test]
fn configured_actor_binding_id_is_its_id() {
    let a = ActorRef::new("lambda", "alice");
    assert!(!a.is_anonymous());
    assert_eq!(a.binding_id(), Some("alice"));
}

#[test]
fn actor_label_collapses_only_the_plain_actor_kind() {
    assert_eq!(ActorRef::new("actor", "svc:build").label(), "svc:build");
    assert_eq!(ActorRef::new("agent", "x").label(), "agent:x");
    assert_eq!(
        ActorRef::new("anonymous", "local").label(),
        "anonymous:local"
    );
}

#[test]
fn split_stamped_label_splits_only_runtime_stamped_kinds() {
    assert_eq!(
        split_stamped_label("actor:svc:build"),
        Some(("actor", "svc:build"))
    );
    assert_eq!(split_stamped_label("agent:x"), Some(("agent", "x")));
    assert_eq!(split_stamped_label("svc:build"), None);
    assert_eq!(split_stamped_label("plain"), None);
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

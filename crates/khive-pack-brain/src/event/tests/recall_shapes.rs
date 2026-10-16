#[test]
fn recall_success_with_target_is_hit() {
    let id = Uuid::new_v4();
    let e = make_event("recall", EventOutcome::Success, Some(id));
    match interpret(&e) {
        BrainSignal::RecallHit { target_id, .. } => assert_eq!(target_id, id),
        other => panic!("expected RecallHit, got {other:?}"),
    }
}

#[test]
fn memory_recall_success_with_target_is_hit() {
    // Namespaced verb, as actually dispatched by the runtime's VerbRegistry
    // for `memory.recall` — see khive-runtime/src/pack.rs's dispatch hook.
    let id = Uuid::new_v4();
    let e = make_event("memory.recall", EventOutcome::Success, Some(id));
    match interpret(&e) {
        BrainSignal::RecallHit { target_id, .. } => assert_eq!(target_id, id),
        other => panic!("expected RecallHit, got {other:?}"),
    }
}

#[test]
fn recall_hit_decodes_serving_profile_attribution() {
    let id = Uuid::new_v4();
    let mut e = make_event("memory.recall", EventOutcome::Success, Some(id));
    e.payload = serde_json::json!({
        "served_by_profile_id": "custom-recall-v1",
        "serve_attribution": "profile"
    });
    match interpret(&e) {
        BrainSignal::RecallHit {
            served_by_profile_id,
            serve_attribution,
            ..
        } => {
            assert_eq!(served_by_profile_id.as_deref(), Some("custom-recall-v1"));
            assert_eq!(serve_attribution, ServeAttribution::Profile);
        }
        other => panic!("expected RecallHit, got {other:?}"),
    }
}

#[test]
fn recall_hit_decodes_explicit_unattributed_marker() {
    let id = Uuid::new_v4();
    let mut e = make_event("memory.recall", EventOutcome::Success, Some(id));
    e.payload = serde_json::json!({"serve_attribution": "unattributed"});
    match interpret(&e) {
        BrainSignal::RecallHit {
            served_by_profile_id,
            serve_attribution,
            ..
        } => {
            assert!(served_by_profile_id.is_none());
            assert_eq!(serve_attribution, ServeAttribution::Unattributed);
        }
        other => panic!("expected RecallHit, got {other:?}"),
    }
}

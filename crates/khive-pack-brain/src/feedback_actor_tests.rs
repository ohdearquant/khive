use super::*;

fn make_pack_with_actor(actor_id: Option<&str>) -> (BrainPack, KhiveRuntime) {
    let rt = KhiveRuntime::new(khive_runtime::RuntimeConfig {
        db_path: None,
        packs: vec!["kg".to_string()],
        brain_profile: None,
        actor_id: actor_id.map(|s| s.to_string()),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime");
    let pack = BrainPack::new(rt.clone());
    (pack, rt)
}

/// Dispatch `brain.feedback` with `signal` and return the emitted
/// `FeedbackExplicit` event's `actor` field, read back from the event store.
async fn feedback_event_actor(
    pack: &BrainPack,
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    signal: &str,
) -> String {
    let registry = empty_registry();
    let target = create_test_entity(rt, token).await;
    let result = pack
        .dispatch(
            "brain.feedback",
            json!({"target_id": target, "signal": signal}),
            &registry,
            token,
        )
        .await
        .expect("brain.feedback must succeed");
    let event_id = uuid::Uuid::parse_str(
        result["event_id"]
            .as_str()
            .expect("response must carry event_id"),
    )
    .expect("event_id must be a uuid");
    let event = rt
        .events(token)
        .expect("event store")
        .get_event(event_id)
        .await
        .expect("get_event must not error")
        .expect("emitted event must be readable back");
    event.actor
}

#[tokio::test]
async fn explicit_signal_actor_reflects_resolved_identity_not_hardcoded_brain() {
    // "useful" is not in `FeedbackEventKind::from_signal_str`, so this
    // exercises the ungated explicit/correction append path.
    let (pack, rt) = make_pack_with_actor(Some("seat:tester"));
    let token = rt.authorize(Namespace::local()).unwrap();
    let actor = feedback_event_actor(&pack, &rt, &token, "useful").await;
    assert_eq!(
        actor, "actor:seat:tester",
        "FeedbackExplicit actor must reflect the resolved caller identity, \
             not a hardcoded \"brain\""
    );
}

#[tokio::test]
async fn implicit_signal_actor_reflects_resolved_identity_not_hardcoded_brain() {
    // "implicit_positive" routes through the gated fold-gate append path
    // (`apply_fold_gate_and_append_event`'s `'static` closure), a second,
    // independent call site that hardcoded the same literal.
    let (pack, rt) = make_pack_with_actor(Some("seat:tester"));
    let token = rt.authorize(Namespace::local()).unwrap();
    let actor = feedback_event_actor(&pack, &rt, &token, "implicit_positive").await;
    assert_eq!(
        actor, "actor:seat:tester",
        "gated-implicit FeedbackExplicit actor must reflect the resolved caller identity"
    );
}

#[tokio::test]
async fn unresolved_actor_falls_back_explicitly_never_to_hardcoded_brain() {
    // No `actor_id` configured -> `resolve_actor` yields `ActorRef::anonymous()`.
    // The fallback must be the explicit "anonymous:local" label, never a
    // silent, plausible-looking "brain" stand-in.
    let (pack, rt) = make_pack_with_actor(None);
    let token = rt.authorize(Namespace::local()).unwrap();
    let actor = feedback_event_actor(&pack, &rt, &token, "implicit_positive").await;
    assert_eq!(
        actor, "anonymous:local",
        "unresolved actor must fall back to an explicit label, never the literal \"brain\""
    );
}

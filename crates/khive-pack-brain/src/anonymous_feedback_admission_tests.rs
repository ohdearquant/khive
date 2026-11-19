use super::*;

const EXPLICIT_SIGNALS: [&str; 6] = [
    "useful",
    "not_useful",
    "wrong",
    "explicit_positive",
    "explicit_negative",
    "correction",
];

fn feedback_args(verb: &str, target: &str, signal: &str) -> Value {
    let mut args = json!({"target_id": target, "signal": signal});
    if verb == "brain.auto_feedback" {
        args["query"] = json!("anonymous feedback admission");
        args["results"] = json!([{"id": target}]);
    }
    args
}

/// Include live posteriors, the durable snapshot and both event stores so
/// refusal cannot append an audit record or silently change replay state.
async fn feedback_state(pack: &BrainPack, rt: &KhiveRuntime, token: &NamespaceToken) -> Value {
    let durable = crate::persist::load_latest_snapshot(
        rt.sql().as_ref(),
        token.namespace().as_str(),
        ENTITY_CACHE_CAPACITY,
    )
    .await
    .expect("read durable brain snapshot");
    let replay =
        crate::persist::load_events_since(rt.sql().as_ref(), token.namespace().as_str(), 0)
            .await
            .expect("read brain replay log");
    let public = rt
        .events(token)
        .expect("event store")
        .query_events(
            khive_storage::event::EventFilter {
                kinds: vec![khive_types::EventKind::FeedbackExplicit],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                limit: 1000,
                offset: 0,
            },
        )
        .await
        .expect("read public feedback events");
    json!({
        "live": pack.snapshot(),
        "durable": durable,
        "replay": replay.events,
        "skipped_replay_rows": replay.quarantined.len(),
        "public": public.items,
    })
}

#[tokio::test]
async fn anonymous_explicit_feedback_refuses_all_signals_and_profiles_without_writes() {
    let (pack, rt) = make_anonymous_pack();
    let token = rt.authorize(Namespace::local()).expect("anonymous token");
    assert!(token.actor().is_anonymous());
    let registry = empty_registry();
    let target = create_test_entity(&rt, &token).await;
    pack.dispatch(
        "brain.create_profile",
        json!({"name": "admission-named", "consumer_kind": "recall"}),
        &registry,
        &token,
    )
    .await
    .expect("create custom profile");
    pack.dispatch(
        "brain.activate",
        json!({"profile_id": "admission-named"}),
        &registry,
        &token,
    )
    .await
    .expect("activate custom profile");

    // Seed real prior judgments so unchanged-state assertions also protect
    // existing events and non-prior posteriors, not just an empty store.
    let (_, attributed_rt) = make_pack();
    let attributed_token = attributed_rt
        .authorize(Namespace::local())
        .expect("attributed seed token");
    for profile in ["balanced-recall-v1", "admission-named"] {
        pack.dispatch(
            "brain.feedback",
            json!({
                "target_id": target,
                "signal": "useful",
                "served_by_profile_id": profile,
            }),
            &registry,
            &attributed_token,
        )
        .await
        .expect("attributed seed feedback");
    }
    let before = feedback_state(&pack, &rt, &token).await;
    assert_eq!(before["public"].as_array().unwrap().len(), 2);

    for verb in ["brain.feedback", "brain.auto_feedback"] {
        for signal in EXPLICIT_SIGNALS {
            for profile in [None, Some("balanced-recall-v1"), Some("admission-named")] {
                let mut args = feedback_args(verb, &target, signal);
                if let Some(profile) = profile {
                    args["served_by_profile_id"] = json!(profile);
                }
                let error = pack
                    .dispatch(verb, args, &registry, &token)
                    .await
                    .expect_err("anonymous explicit feedback must be refused");
                assert_anonymous_feedback_refusal(error);
                assert_eq!(
                    feedback_state(&pack, &rt, &token).await,
                    before,
                    "refusal changed state: {verb}, {signal}, {profile:?}"
                );
            }
        }
    }

    // A full UUID avoids prefix resolution; caller admission must happen
    // before target existence or named-profile resolution can decide it.
    let missing_target = uuid::Uuid::new_v4().to_string();
    for verb in ["brain.feedback", "brain.auto_feedback"] {
        let mut args = feedback_args(verb, &missing_target, "correction");
        args["served_by_profile_id"] = json!("missing-profile");
        let error = pack
            .dispatch(verb, args, &registry, &token)
            .await
            .expect_err("anonymous admission precedes existence checks");
        assert_anonymous_feedback_refusal(error);
    }
    assert_eq!(feedback_state(&pack, &rt, &token).await, before);
}

#[tokio::test]
async fn configured_local_actor_explicit_feedback_is_refused_like_anonymous() {
    // `actor.id = "local"` names the unattributed pool, not a caller.
    let (pack, rt) = make_pack_with_actor("local");
    let token = rt.authorize(Namespace::local()).expect("local token");
    assert!(
        !token.actor().is_anonymous(),
        "test setup: configured actor"
    );
    assert!(khive_runtime::actor_is_unattributed(token.actor()));
    let registry = empty_registry();
    let target = create_test_entity(&rt, &token).await;
    // Seed one attributed judgment so the default profile is persisted;
    // an unpersisted default is minted per snapshot with a fresh timestamp.
    let (_, attributed_rt) = make_pack();
    let attributed_token = attributed_rt
        .authorize(Namespace::local())
        .expect("attributed seed token");
    pack.dispatch(
        "brain.feedback",
        json!({
            "target_id": target,
            "signal": "useful",
            "served_by_profile_id": "balanced-recall-v1",
        }),
        &registry,
        &attributed_token,
    )
    .await
    .expect("attributed seed feedback");
    let before = feedback_state(&pack, &rt, &token).await;
    assert_eq!(before["public"].as_array().unwrap().len(), 1);
    for verb in ["brain.feedback", "brain.auto_feedback"] {
        for signal in EXPLICIT_SIGNALS {
            let error = pack
                .dispatch(
                    verb,
                    feedback_args(verb, &target, signal),
                    &registry,
                    &token,
                )
                .await
                .expect_err("unattributed explicit feedback must be refused");
            assert_anonymous_feedback_refusal(error);
            assert_eq!(
                feedback_state(&pack, &rt, &token).await,
                before,
                "{verb} {signal}"
            );
        }
    }
}

#[tokio::test]
async fn anonymous_implicit_feedback_still_emits_and_trains_the_default_profile() {
    for verb in ["brain.feedback", "brain.auto_feedback"] {
        for signal in ["implicit_positive", "implicit_negative"] {
            let (pack, rt) = make_anonymous_pack();
            let token = rt.authorize(Namespace::local()).expect("anonymous token");
            let registry = empty_registry();
            let target = create_test_entity(&rt, &token).await;
            let result = pack
                .dispatch(
                    verb,
                    feedback_args(verb, &target, signal),
                    &registry,
                    &token,
                )
                .await
                .expect("anonymous implicit feedback retains its existing admission");
            assert_eq!(result["emitted"], json!(true));
            assert_eq!(pack.snapshot().balanced_recall.total_events, 1);
            let event_id = result["event_id"].as_str().unwrap().parse().unwrap();
            let event = rt
                .events(&token)
                .expect("event store")
                .get_event(event_id)
                .await
                .expect("read implicit event")
                .expect("implicit event exists");
            assert_eq!(event.actor, "anonymous:local");
            assert_eq!(
                event.payload["served_by_profile_id"],
                json!("balanced-recall-v1")
            );
        }
    }
}

#[tokio::test]
async fn attributed_explicit_feedback_without_profile_keeps_default_routing() {
    // A configured actor id other than the unattributed pool name is
    // attributed; the pool name "local" is refused (see the test above).
    for actor in ["brain-test", "agent-x"] {
        let (pack, rt) = make_pack_with_actor(actor);
        let token = rt.authorize(Namespace::local()).expect("attributed token");
        assert!(!token.actor().is_anonymous());
        let registry = empty_registry();
        let mut expected_total = 0;
        for verb in ["brain.feedback", "brain.auto_feedback"] {
            for signal in EXPLICIT_SIGNALS {
                let target = create_test_entity(&rt, &token).await;
                let result = pack
                    .dispatch(
                        verb,
                        feedback_args(verb, &target, signal),
                        &registry,
                        &token,
                    )
                    .await
                    .expect("attributed feedback can use the default profile");
                assert_eq!(result["emitted"], json!(true));
                expected_total += 1;
                assert_eq!(pack.snapshot().balanced_recall.total_events, expected_total);
                let event_id = result["event_id"].as_str().unwrap().parse().unwrap();
                let event = rt
                    .events(&token)
                    .expect("event store")
                    .get_event(event_id)
                    .await
                    .expect("read attributed event")
                    .expect("attributed event exists");
                assert_eq!(event.actor, format!("actor:{actor}"));
                assert_eq!(
                    event.payload["served_by_profile_id"],
                    json!("balanced-recall-v1")
                );
            }
        }
    }
}

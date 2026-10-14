use super::*;

#[tokio::test]
async fn semantic_feedback_updates_sections_in_live_and_reloaded_snapshot() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    let before = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .expect("read profile before feedback");
    let salience_alpha_before = before["state_snapshot"]["salience"]["alpha"]
        .as_f64()
        .expect("salience alpha");

    pack.dispatch(
        "brain.feedback",
        json!({
            "target_id": target,
            "signal": "explicit_positive",
            "served_by_profile_id": "balanced-recall-v1",
            "section_signals": {
                "overview": "wrong",
                "formalism": "useful"
            }
        }),
        &registry,
        &token,
    )
    .await
    .expect("semantic feedback with section signals");

    let live = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .expect("read profile after feedback");
    assert_eq!(live["section_posteriors"]["overview"]["alpha"], json!(2.0));
    assert_eq!(live["section_posteriors"]["overview"]["beta"], json!(5.0));
    assert_eq!(live["section_posteriors"]["formalism"]["alpha"], json!(3.0));
    assert_eq!(live["section_posteriors"]["formalism"]["beta"], json!(4.0));
    assert_eq!(live["section_posteriors"]["examples"]["alpha"], json!(5.0));
    assert_eq!(
        live["state_snapshot"]["salience"]["alpha"],
        json!(salience_alpha_before + 1.5)
    );
    assert_eq!(
        live["state_snapshot"]["temporal"],
        before["state_snapshot"]["temporal"]
    );

    let reloaded = BrainPack::new(rt.clone())
        .dispatch(
            "brain.profile",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .expect("reload durable profile snapshot");
    assert_eq!(reloaded["section_posteriors"], live["section_posteriors"]);
    assert_eq!(reloaded["state_snapshot"], live["state_snapshot"]);
}

#[tokio::test]
async fn semantic_section_signals_replay_after_snapshot() {
    use khive_brain_core::BrainState;
    use khive_storage::event::Event;
    use khive_types::{EventKind, SubstrateKind};
    use uuid::Uuid;

    let (_, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let namespace = token.namespace().as_str();
    let initial = BrainState::new(crate::ENTITY_CACHE_CAPACITY);
    let salience_beta_before = initial.balanced_recall.salience.beta();
    let relevance_beta_before = initial.balanced_recall.relevance.beta();
    let temporal_before = json!(initial.balanced_recall.temporal);
    let snapshot_time_us = 1_000;
    crate::persist::upsert_snapshot(
        rt.sql().as_ref(),
        namespace,
        &initial.to_snapshot(),
        snapshot_time_us,
    )
    .await
    .expect("persist pre-feedback snapshot");

    let mut event = Event::new(
        namespace,
        "brain.feedback",
        EventKind::Audit,
        SubstrateKind::Event,
        "brain",
    );
    event.target_id = Some(Uuid::new_v4());
    event.payload = json!({
        "signal": "correction",
        "served_by_profile_id": "balanced-recall-v1",
        "section_signals": {
            "overview": "useful",
            "formalism": "wrong"
        }
    });
    crate::persist::append_brain_event(
        rt.sql().as_ref(),
        namespace,
        "balanced-recall-v1",
        "brain.feedback",
        &serde_json::to_value(event).expect("serialize feedback event"),
        snapshot_time_us + 1_000,
    )
    .await
    .expect("append feedback after snapshot");

    let replayed = BrainPack::new(rt.clone())
        .dispatch(
            "brain.profile",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .expect("reload snapshot and replay feedback");
    assert_eq!(
        replayed["section_posteriors"]["overview"]["alpha"],
        json!(4.0)
    );
    assert_eq!(
        replayed["section_posteriors"]["overview"]["beta"],
        json!(2.0)
    );
    assert_eq!(
        replayed["section_posteriors"]["formalism"]["alpha"],
        json!(1.5)
    );
    assert_eq!(
        replayed["section_posteriors"]["formalism"]["beta"],
        json!(8.0)
    );
    assert_eq!(
        replayed["state_snapshot"]["salience"]["beta"],
        json!(salience_beta_before + 2.0)
    );
    assert_eq!(
        replayed["state_snapshot"]["relevance"]["beta"],
        json!(relevance_beta_before + 2.0)
    );
    assert_eq!(replayed["state_snapshot"]["temporal"], temporal_before);
}

use super::*;
use khive_brain_core::{SectionPosteriorState, SectionType};

#[tokio::test]
async fn feedback_section_signals_seeds_balanced_recall_section_state() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    // Before any feedback: `balanced-recall-v1` must NOT have a section_states
    // entry — that is the precondition the fix corrects.
    {
        let state = pack.state.lock().unwrap();
        assert!(
            !state.section_states.contains_key("balanced-recall-v1"),
            "section_states must not contain balanced-recall-v1 before first feedback"
        );
    }

    // Fire feedback with section_signals; no `served_by_profile_id` defaults
    // to `balanced-recall-v1`.
    pack.dispatch(
        "brain.feedback",
        json!({
            "target_id": target,
            "signal": "useful",
            "section_signals": {
                "operational_guidance": "useful"
            }
        }),
        &registry,
        &token,
    )
    .await
    .expect("feedback must succeed");

    // After feedback:
    // (a) balanced-recall-v1 must now have a section_states entry.
    // (b) The entry must be COMPLETE — all SectionType slots present.
    // (c) The OperationalGuidance posterior mean must have moved above the
    //     default prior (useful signal → alpha++).
    let state = pack.state.lock().unwrap();
    let ss = state
        .section_states
        .get("balanced-recall-v1")
        .expect("balanced-recall-v1 must have a section_states entry after first feedback");

    assert_eq!(
        ss.posteriors.len(),
        SectionType::all().len(),
        "section state must contain all {} SectionType slots after seeding",
        SectionType::all().len()
    );

    let default_og_mean = SectionPosteriorState::default_priors()
        .get(&SectionType::OperationalGuidance)
        .expect("default_priors must include OperationalGuidance")
        .mean();
    let live_og_mean = ss
        .posteriors
        .get(&SectionType::OperationalGuidance)
        .expect("OperationalGuidance must be present after seeding")
        .mean();

    assert!(
        live_og_mean > default_og_mean,
        "OperationalGuidance mean must increase after useful signal; \
             default={default_og_mean:.4} live={live_og_mean:.4}"
    );
}

#[tokio::test]
async fn semantic_feedback_moves_section_posterior_on_live_handler_path() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;
    let before = SectionPosteriorState::default_priors()[&SectionType::OperationalGuidance].alpha();

    pack.dispatch(
        "brain.feedback",
        json!({
            "target_id": target,
            "signal": "explicit_positive",
            "section_signals": {"operational_guidance": "useful"}
        }),
        &registry,
        &token,
    )
    .await
    .expect("semantic feedback with a section judgment must succeed");

    let state = pack.state.lock().unwrap();
    let section = state
        .section_states
        .get("balanced-recall-v1")
        .expect("semantic feedback must seed section state");
    let alpha = section.posteriors[&SectionType::OperationalGuidance].alpha();
    assert!((alpha - before - 1.5).abs() < 1e-12);
}

#[tokio::test]
async fn feedback_section_signals_updates_custom_profile_with_seeded_priors() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    // Create a custom profile with a high-alpha seed prior for Formalism.
    // This verifies that the seeding path (create_profile) and the live
    // feedback path are consistent: the seeded section state is preserved
    // and further updated by feedback.
    pack.dispatch(
        "brain.create_profile",
        json!({
            "name": "test-section-custom",
            "description": "custom profile for router section-posterior regression",
            "consumer_kind": "recall",
            "seed_priors": {
                "section_posteriors": {
                    "formalism": {"alpha": 8.0, "beta": 2.0}
                }
            }
        }),
        &registry,
        &token,
    )
    .await
    .expect("brain.create_profile must succeed");

    // Record the Formalism mean from the seeded state before any feedback.
    let formalism_seeded_mean = {
        let state = pack.state.lock().unwrap();
        state
            .section_states
            .get("test-section-custom")
            .expect("test-section-custom must have section_states after create_profile")
            .posteriors
            .get(&SectionType::Formalism)
            .expect("Formalism must be present in seeded state")
            .mean()
    };

    // Fire feedback targeting the custom profile with a useful Formalism signal.
    pack.dispatch(
        "brain.feedback",
        json!({
            "target_id": target,
            "signal": "useful",
            "served_by_profile_id": "test-section-custom",
            "section_signals": {
                "formalism": "useful"
            }
        }),
        &registry,
        &token,
    )
    .await
    .expect("feedback to custom profile must succeed");

    // The Formalism posterior mean must have increased from its seeded value.
    let formalism_live_mean = {
        let state = pack.state.lock().unwrap();
        state
            .section_states
            .get("test-section-custom")
            .expect("test-section-custom must still have section_states after feedback")
            .posteriors
            .get(&SectionType::Formalism)
            .expect("Formalism must still be present after feedback")
            .mean()
    };

    assert!(
        formalism_live_mean > formalism_seeded_mean,
        "Formalism posterior mean must increase after useful signal; \
             seeded={formalism_seeded_mean:.4} live={formalism_live_mean:.4}"
    );
}

// ── Replay regression: section posteriors seeded on snapshot reload ────────
//
// Regression gate for the persisted event-replay path (persist.rs).
//
// Before the fix, the replay loop used `if let Some(..) = section_states.get_mut(..)`
// which silently dropped section signals whenever the snapshot had no entry for the
// serving profile.  After a restart the live state then diverged from the event log.
//
// This test constructs the divergence scenario explicitly:
//   1. Write a snapshot with empty section_states to the DB (simulates a snapshot
//      from before section_states was introduced, or before the first feedback).
//   2. Append a brain.feedback event with section_signals after the snapshot timestamp
//      so the replay loop will pick it up.
//   3. Create a fresh BrainPack over the same runtime (no pre-loaded state) and
//      trigger ensure_loaded via a dispatch.
//   4. Assert that the replayed state has all SectionType slots seeded AND that the
//      signalled section's posterior mean moved above the default prior.

#[tokio::test]
async fn replay_seeds_section_posteriors_for_missing_profile() {
    use khive_brain_core::BrainState;
    use khive_storage::event::Event as StorageEvent;
    use khive_types::{EventKind, SubstrateKind};
    use uuid::Uuid;

    let rt = khive_runtime::KhiveRuntime::memory().expect("in-memory runtime");
    let token = rt.authorize(khive_runtime::Namespace::local()).unwrap();
    let namespace = token.namespace().as_str();

    // Build and persist a snapshot with empty section_states at time T0.
    // BrainState::new() leaves section_states empty, which is the condition
    // that triggered the silent-drop bug in the old replay path.
    let snapshot = BrainState::new(crate::ENTITY_CACHE_CAPACITY).to_snapshot();
    let t0_us: i64 = 1_000;
    crate::persist::upsert_snapshot(rt.sql().as_ref(), namespace, &snapshot, t0_us)
        .await
        .expect("upsert snapshot");

    // Append a brain.feedback event with section_signals at time T1 > T0.
    // The full Event struct is serialized to JSON (same wire format as
    // persist_after_feedback writes during a live dispatch).
    let mut ev = StorageEvent::new(
        namespace,
        "brain.feedback",
        EventKind::Audit,
        SubstrateKind::Event,
        "brain",
    );
    ev.target_id = Some(Uuid::new_v4());
    ev.payload = serde_json::json!({
        "signal": "useful",
        "section_signals": {"operational_guidance": "useful"},
    });
    let event_value = serde_json::to_value(&ev).expect("serialize event");
    let t1_us: i64 = t0_us + 1_000;
    crate::persist::append_brain_event(
        rt.sql().as_ref(),
        namespace,
        "balanced-recall-v1",
        "brain.feedback",
        &event_value,
        t1_us,
    )
    .await
    .expect("append brain event");

    // Fresh BrainPack with no pre-loaded state — ensure_loaded will do the
    // snapshot load + event replay on the first dispatch.
    let pack2 = crate::BrainPack::new(rt.clone());
    let registry = empty_registry();
    pack2
        .dispatch("brain.profiles", serde_json::json!({}), &registry, &token)
        .await
        .expect("brain.profiles dispatch after replay");

    // Verify that the replay path (now using ensure_section_state_seeded)
    // produced a fully-seeded entry for balanced-recall-v1.
    let state = pack2.state.lock().unwrap();
    let ss = state
        .section_states
        .get("balanced-recall-v1")
        .expect("balanced-recall-v1 must have a section_states entry after replay seeding");

    assert_eq!(
        ss.posteriors.len(),
        SectionType::all().len(),
        "replay must seed all {} SectionType slots; got {}",
        SectionType::all().len(),
        ss.posteriors.len()
    );

    let default_og_mean = SectionPosteriorState::default_priors()
        .get(&SectionType::OperationalGuidance)
        .expect("default_priors must include OperationalGuidance")
        .mean();
    let replayed_og_mean = ss
        .posteriors
        .get(&SectionType::OperationalGuidance)
        .expect("OperationalGuidance must be present after replay seeding")
        .mean();

    assert!(
        replayed_og_mean > default_og_mean,
        "OperationalGuidance mean must increase after useful replay signal; \
             default={default_og_mean:.4} replayed={replayed_og_mean:.4}"
    );
}

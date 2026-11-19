//! Retired section types in recorded events and live writes (ADR-048, 2026-10-04
//! amendment): replay drops the retired entries and applies the rest; live writes
//! keep refusing a retired name as unknown.

use khive_brain_core::{
    BrainSignal, BrainState, FeedbackSignal, SectionPosteriorState, SectionType,
};
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use khive_storage::event::Event;
use khive_types::{EventKind, SubstrateKind};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::event::interpret;
use crate::{BrainPack, ENTITY_CACHE_CAPACITY};

fn feedback_event(signals: Value) -> Event {
    let mut event = Event::new(
        "local",
        "brain.feedback",
        EventKind::Audit,
        SubstrateKind::Event,
        "brain",
    );
    event.target_id = Some(Uuid::new_v4());
    event.payload = json!({"signal": "useful", "section_signals": signals});
    event
}

fn section_feedback_event(signals: Value) -> Event {
    let mut event = Event::new(
        "local",
        "brain.section_feedback",
        EventKind::FeedbackExplicit,
        SubstrateKind::Event,
        "actor:test",
    );
    event.payload = json!({
        "served_by_profile_id": "balanced-recall-v1",
        "section_signals": signals,
    });
    event
}

#[test]
fn replay_drops_retired_keys_and_keeps_the_rest() {
    let kept = crate::replay_section_signals(&json!({
        "overview": "useful",
        "references": "wrong",
        "other": "useful",
    }))
    .expect("one current key remains");
    assert_eq!(kept, json!({"overview": "useful"}));

    assert_eq!(
        crate::replay_section_signals(&json!({"references": "useful", "other": "wrong"})),
        None,
        "a map of retired keys alone leaves no section evidence"
    );
    // Anything else is left for the strict validator to refuse.
    assert_eq!(
        crate::replay_section_signals(&json!({"glossary": "useful", "references": "useful"})),
        Some(json!({"glossary": "useful"}))
    );
    assert_eq!(crate::replay_section_signals(&json!({})), Some(json!({})));
    assert_eq!(
        crate::replay_section_signals(&json!(["references"])),
        Some(json!(["references"]))
    );
}

#[test]
fn interpret_applies_the_remaining_signals_of_a_recorded_feedback_event() {
    let event = feedback_event(json!({"overview": "useful", "references": "wrong"}));
    match interpret(&event) {
        BrainSignal::Feedback {
            signal,
            section_signals,
            ..
        } => {
            assert_eq!(signal, FeedbackSignal::Useful);
            let signals = section_signals.expect("the current key's signal survives");
            assert_eq!(signals.len(), 1);
            assert_eq!(signals[&SectionType::Overview], FeedbackSignal::Useful);
        }
        other => panic!("expected Feedback, got {other:?}"),
    }
}

#[test]
fn interpret_keeps_the_scalar_signal_when_every_section_key_was_retired() {
    let event = feedback_event(json!({"references": "useful"}));
    match interpret(&event) {
        BrainSignal::Feedback {
            signal,
            section_signals,
            ..
        } => {
            assert_eq!(signal, FeedbackSignal::Useful);
            assert!(section_signals.is_none());
        }
        other => panic!("expected Feedback, got {other:?}"),
    }
}

#[test]
fn interpret_still_refuses_a_map_with_another_unknown_key() {
    for signals in [
        json!({"references": "useful", "glossary": "useful"}),
        json!({"overview": "useful", "glossary": "useful"}),
    ] {
        match interpret(&feedback_event(signals.clone())) {
            BrainSignal::Feedback {
                section_signals, ..
            } => assert!(section_signals.is_none(), "{signals} must be refused"),
            other => panic!("expected Feedback, got {other:?}"),
        }
    }
}

#[test]
fn section_feedback_event_replays_its_remaining_signals() {
    let event = section_feedback_event(json!({"overview": "useful", "references": "wrong"}));
    let (profile, signals) =
        crate::section_feedback::decode_event(&event).expect("a retired key is not an error");
    assert_eq!(profile, "balanced-recall-v1");
    assert_eq!(signals.len(), 1);
    assert_eq!(signals[&SectionType::Overview], FeedbackSignal::Useful);

    let mut state = BrainState::new(ENTITY_CACHE_CAPACITY);
    crate::section_feedback::replay(&mut state, &event).expect("replay applies the rest");
    let section_state = &state.section_states["balanced-recall-v1"];
    let default_priors = SectionPosteriorState::default_priors();
    assert_eq!(section_state.total_events, 1);
    assert_eq!(
        section_state.posteriors[&SectionType::Overview].alpha(),
        default_priors[&SectionType::Overview].alpha() + 1.0
    );
}

#[test]
fn section_feedback_event_of_only_retired_keys_is_a_no_op_not_a_quarantine() {
    let event = section_feedback_event(json!({"references": "useful", "other": "wrong"}));
    let (_, signals) =
        crate::section_feedback::decode_event(&event).expect("decodes without error");
    assert!(signals.is_empty());

    let mut state = BrainState::new(ENTITY_CACHE_CAPACITY);
    crate::section_feedback::replay(&mut state, &event).expect("replay succeeds");
    assert!(
        !state.section_states.contains_key("balanced-recall-v1"),
        "nothing remained to apply, so no section state is created or advanced"
    );
}

/// The posterior fold counts an event only when a signal remains to apply, so an
/// event whose every key was retired leaves the event count and the exploration
/// epoch where they were.
#[test]
fn section_posterior_fold_skips_an_event_of_only_retired_keys() {
    use khive_fold::{Fold, FoldContext};

    let fold = crate::fold::SectionPosteriorFold::new();
    let ctx = FoldContext::new();
    let initial = fold.init(&ctx);
    let (events, epoch) = (initial.total_events, initial.exploration_epoch);
    assert!(
        epoch > 0,
        "the default exploration epoch must be able to move"
    );

    let retired = section_feedback_event(json!({"references": "useful", "other": "wrong"}));
    let state = fold.reduce(initial, &retired, &ctx);
    assert_eq!(state.total_events, events);
    assert_eq!(state.exploration_epoch, epoch);

    let live = section_feedback_event(json!({"overview": "useful"}));
    let state = fold.reduce(state, &live, &ctx);
    assert_eq!(
        state.total_events,
        events + 1,
        "a live key reaches the counter"
    );
    assert_eq!(state.exploration_epoch, epoch - 1);
}

#[test]
fn section_feedback_event_with_another_unknown_key_is_still_refused() {
    let event = section_feedback_event(json!({"references": "useful", "glossary": "useful"}));
    crate::section_feedback::decode_event(&event).expect_err("glossary is not a section type");
}

/// A recorded event newer than the snapshot is replayed on load. Its retired key
/// is dropped, the event is not quarantined, and the current key's signal moves
/// the posterior.
#[tokio::test]
async fn persisted_feedback_event_with_a_retired_key_replays_without_quarantine() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = rt.authorize(Namespace::local()).unwrap();
    let namespace = token.namespace().as_str();

    let snapshot = BrainState::new(ENTITY_CACHE_CAPACITY).to_snapshot();
    let t0_us: i64 = 1_000;
    crate::persist::upsert_snapshot(rt.sql().as_ref(), namespace, &snapshot, t0_us)
        .await
        .expect("upsert snapshot");

    let signals = json!({"operational_guidance": "useful", "references": "useful"});
    let event = feedback_event(signals);
    crate::persist::append_brain_event(
        rt.sql().as_ref(),
        namespace,
        "balanced-recall-v1",
        "brain.feedback",
        &serde_json::to_value(&event).expect("serialize event"),
        t0_us + 1_000,
    )
    .await
    .expect("append brain event");

    let replay = crate::persist::load_events_since(rt.sql().as_ref(), namespace, t0_us)
        .await
        .expect("load events");
    assert_eq!(replay.quarantine_count(), 0);
    assert!(replay.quarantined.is_empty());
    assert_eq!(replay.events.len(), 1);

    let pack = BrainPack::new(rt.clone());
    crate::persist::ensure_loaded(
        &rt,
        &token,
        &pack.persistence,
        &pack.state,
        ENTITY_CACHE_CAPACITY,
    )
    .await
    .expect("load and replay");

    let state = pack.state.lock().unwrap();
    let section_state = state
        .section_states
        .get("balanced-recall-v1")
        .expect("replay seeds the serving profile's section state");
    assert_eq!(section_state.posteriors.len(), SectionType::all().len());
    let default_priors = SectionPosteriorState::default_priors();
    let default_mean = default_priors[&SectionType::OperationalGuidance].mean();
    assert!(
        section_state.posteriors[&SectionType::OperationalGuidance].mean() > default_mean,
        "the current key's useful signal must have applied"
    );
}

fn live_runtime() -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        actor_id: Some("test:retired".to_owned()),
        brain_profile: None,
        packs: vec!["kg".into(), "brain".into()],
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap()
}

fn live_registry(runtime: &KhiveRuntime) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    khive_runtime::PackRegistry::register_packs(
        &["kg".into(), "brain".into()],
        runtime.clone(),
        &mut builder,
    )
    .unwrap();
    builder.with_actor_id(runtime.config().actor_id.clone());
    builder.build().unwrap()
}

/// Live writes keep refusing a retired name as an unknown section type, even
/// though replay now tolerates it.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn live_brain_feedback_refuses_a_retired_section_signal() {
    let rt = live_runtime();
    let registry = live_registry(&rt);
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = rt
        .create_entity_with_embedding_report(
            &token,
            "concept",
            None,
            "test-target",
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .expect("create test entity")
        .id
        .to_string();

    let control = registry
        .dispatch(
            "brain.feedback",
            json!({
                "target_id": target,
                "signal": "useful",
                "section_signals": {"overview": "useful"},
            }),
        )
        .await;
    assert!(
        control.is_ok(),
        "a current section type is accepted: {control:?}"
    );

    for retired in SectionType::RETIRED_NAMES {
        let mut signals = Map::new();
        signals.insert((*retired).to_owned(), json!("useful"));
        let error = registry
            .dispatch(
                "brain.feedback",
                json!({
                    "target_id": target,
                    "signal": "useful",
                    "section_signals": signals,
                }),
            )
            .await
            .expect_err("a retired section type is refused on a live write");
        let message = error.to_string();
        assert!(message.contains(retired), "{message}");
        assert!(message.contains("unknown section"), "{message}");
        assert!(
            message.contains("overview"),
            "valid values listed: {message}"
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn create_profile_seed_priors_refuse_a_retired_section_type() {
    let rt = live_runtime();
    let registry = live_registry(&rt);
    let seed = |section: &str| {
        let mut sections = Map::new();
        sections.insert(section.to_owned(), json!({"alpha": 2.0, "beta": 2.0}));
        json!({
            "name": format!("seed-{section}"),
            "consumer_kind": "knowledge_compose",
            "seed_priors": {"section_posteriors": sections},
        })
    };

    registry
        .dispatch("brain.create_profile", seed("overview"))
        .await
        .expect("a current section type seeds a profile");
    for retired in SectionType::RETIRED_NAMES {
        let error = registry
            .dispatch("brain.create_profile", seed(retired))
            .await
            .expect_err("a retired section type is refused as a seed prior");
        assert!(error.to_string().contains(retired), "{error}");
    }
}

//! Integration tests for `BrainPack` dispatch.
#[path = "../../khive-runtime/tests/support/receipt_credentials.rs"]
mod receipt_credentials;

mod auto_feedback_unjudged;
mod resolve_absence_tests;

use super::*;
use khive_runtime::{
    DispatchHook, KhiveRuntime, Namespace, NamespaceToken, PackRuntime, RuntimeConfig,
    RuntimeError, VerbRegistryBuilder,
};
use khive_types::HandlerDef;
use serde_json::{json, Value};

struct TestConsumerPack;

impl khive_types::Pack for TestConsumerPack {
    const NAME: &'static str = "test-consumer";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const BRAIN_CONSUMER_KINDS: &'static [&'static str] =
        &["recall", "knowledge_compose", "search"];
    const HANDLERS: &'static [HandlerDef] = &[];
}

#[async_trait::async_trait]
impl PackRuntime for TestConsumerPack {
    fn name(&self) -> &str {
        <Self as khive_types::Pack>::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <Self as khive_types::Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <Self as khive_types::Pack>::ENTITY_KINDS
    }

    fn brain_consumer_kinds(&self) -> &'static [&'static str] {
        <Self as khive_types::Pack>::BRAIN_CONSUMER_KINDS
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        <Self as khive_types::Pack>::HANDLERS
    }

    async fn dispatch(
        &self,
        verb: &str,
        _params: serde_json::Value,
        _registry: &khive_runtime::VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<serde_json::Value, RuntimeError> {
        Err(RuntimeError::InvalidInput(format!(
            "test consumer pack does not handle verb {verb:?}"
        )))
    }
}

fn make_pack() -> (BrainPack, KhiveRuntime) {
    make_pack_with_actor("brain-test")
}

fn make_anonymous_pack() -> (BrainPack, KhiveRuntime) {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let pack = BrainPack::new(rt.clone());
    (pack, rt)
}

fn assert_anonymous_feedback_refusal(error: RuntimeError) {
    let RuntimeError::Khive(error) = error else {
        panic!("expected typed anonymous-feedback refusal, got {error:?}");
    };
    assert_eq!(error.kind(), khive_types::ErrorKind::InvalidInput);
    assert!(error.message().contains("attributed caller"), "{error}");
    assert!(error.message().contains("actor.id"), "{error}");
}

fn make_pack_with_read_scope(
    actor: Option<&str>,
    visible_actors: &[&str],
    fleet_readers: &[&str],
) -> (BrainPack, KhiveRuntime) {
    let mut config = khive_runtime::KhiveConfig::default();
    config.actor.id = actor.map(str::to_string);
    config.actor.visible_namespaces = Some(visible_actors.iter().map(|s| s.to_string()).collect());
    config.brain.fleet_readers = fleet_readers.iter().map(|s| s.to_string()).collect();
    let rt = KhiveRuntime::new(khive_runtime::runtime_config_from_khive_config(
        &config,
        RuntimeConfig {
            db_path: None,
            packs: vec!["kg".to_string()],
            brain_profile: None,
            actor_id: None,
            ..RuntimeConfig::no_embeddings()
        },
    ))
    .expect("in-memory runtime with actor read scope");
    (BrainPack::new(rt.clone()), rt)
}

/// Like `make_pack`, with the display timezone pinned so date-only
/// `since`/`until` anchoring is deterministic regardless of the host zone
/// (`RuntimeConfig::default()` resolves the machine's own zone).
fn make_pack_with_tz(tz: &str) -> (BrainPack, KhiveRuntime) {
    let rt = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        packs: vec!["kg".to_string()],
        brain_profile: None,
        actor_id: Some("brain-test".to_string()),
        display_timezone: tz.parse().expect("known IANA zone"),
        ..RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime");
    let pack = BrainPack::new(rt.clone());
    (pack, rt)
}

fn empty_registry() -> khive_runtime::VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(TestConsumerPack);
    builder
        .build()
        .expect("handler-empty test registry builds successfully")
}

/// Create a real entity in the runtime and return its UUID string.
/// Used by feedback tests that need a valid target_id.
async fn create_test_entity(rt: &KhiveRuntime, token: &NamespaceToken) -> String {
    let entity = rt
        .create_entity_with_embedding_report(
            token,
            "concept",
            None,
            "test-target",
            None,
            None,
            vec![],
        )
        .await
        .map(|(record, _report)| record)
        .expect("create test entity");
    entity.id.to_string()
}

const LIFECYCLE_PROFILE_ID: &str = "lifecycle-test-v1";

async fn create_active_lifecycle_profile(
    pack: &BrainPack,
    registry: &khive_runtime::VerbRegistry,
    token: &NamespaceToken,
) {
    pack.dispatch(
        "brain.create_profile",
        json!({"name": LIFECYCLE_PROFILE_ID, "consumer_kind": "recall"}),
        registry,
        token,
    )
    .await
    .expect("create lifecycle test profile");
    pack.dispatch(
        "brain.activate",
        json!({"profile_id": LIFECYCLE_PROFILE_ID}),
        registry,
        token,
    )
    .await
    .expect("activate lifecycle test profile");
}

include!("tests/profile_reset_tests.rs");

#[tokio::test]
async fn dispatch_feedback_invalid_signal_returns_invalid_input() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let target = "00000000-0000-0000-0000-000000000001";
    let err = pack
        .dispatch(
            "brain.feedback",
            json!({"target_id": target, "signal": "bad_signal"}),
            &registry,
            &rt.authorize(Namespace::local()).unwrap(),
        )
        .await
        .unwrap_err();
    if let RuntimeError::InvalidInput(msg) = &err {
        assert!(
            msg.contains("bad_signal"),
            "expected signal name in error: {msg}"
        );
        assert!(
            msg.contains("valid"),
            "expected hint about valid values: {msg}"
        );
    } else {
        panic!("expected InvalidInput, got {err:?}");
    }
}

include!("tests/profile_discovery_and_binding_tests.rs");

include!("tests/public_profile_lifecycle_tests.rs");

include!("tests/binding_and_configuration_tests.rs");

// ── Regression tests ──────────────────────────────────────────────────────

// profile_record.total_events must stay synchronized with
// balanced_recall.total_events after both handle_feedback and
// on_dispatch. Each path must synchronize the stored record with the
// live fold state.
//
// Part A — handle_feedback path (unchanged from before).
// Part B — on_dispatch path: introduce a deliberate desync by reaching into
//   the live state directly, then fire on_dispatch to verify the sync
//   corrects it.  This would fail if sync_balanced_recall_record is removed
//   from on_dispatch.
#[tokio::test]
async fn test_356_profile_record_total_events_synced_after_feedback() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    // Part A: handle_feedback path.
    for _ in 0..3 {
        pack.dispatch(
            "brain.feedback",
            json!({"target_id": target, "signal": "useful"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    }

    let snap = pack.snapshot();
    let live_total = snap.balanced_recall.total_events;

    let record_result = pack
        .dispatch(
            "brain.profile",
            json!({"id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    let record_total = record_result["total_events"].as_u64().unwrap();

    assert_eq!(
        live_total, record_total,
        "#356 part-A: profile_record.total_events ({record_total}) must equal \
         balanced_recall.total_events ({live_total}) after feedback calls"
    );
    assert_eq!(live_total, 3, "expected exactly 3 events from part A");

    // Part B: on_dispatch path.
    // Deliberately desync the record by bumping balanced_recall.total_events
    // directly (simulating what would happen if only on_dispatch updated the
    // live state but the sync call were missing).
    {
        let mut state = pack.state.lock().unwrap();
        state.balanced_recall.total_events += 7; // introduce desync
                                                 // Profile record still says `live_total` at this point.
    }

    // Fire on_dispatch with an irrelevant (non-brain) verb event — this is
    // exactly what the runtime hook does for every non-brain verb dispatch.
    // The sync helper inside on_dispatch must correct the desync.
    let hook_event = {
        use khive_types::{EventKind, SubstrateKind};
        let mut e = khive_storage::event::Event::new(
            "local",
            "search",
            EventKind::Audit,
            SubstrateKind::Event,
            "kg",
        );
        e.outcome = khive_types::EventOutcome::Success;
        e
    };
    let hook_view = khive_runtime::EventView {
        event: hook_event,
        observations: Vec::new(),
    };
    pack.on_dispatch(&hook_view).await;

    // After on_dispatch, the record must reflect the new (desynced) live total.
    let after_hook = pack
        .dispatch(
            "brain.profile",
            json!({"id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    let after_total = after_hook["total_events"].as_u64().unwrap();
    let live_after = pack.snapshot().balanced_recall.total_events;
    assert_eq!(
        after_total, live_after,
        "#356 part-B: on_dispatch sync must correct desync; \
         record shows {after_total}, live state shows {live_after}"
    );
}

// brain.feedback must not double-count total_events.
//
// The double-count path: VerbRegistry::dispatch calls the registered pack
// handler (handle_feedback folds once) and then calls on_dispatch on every
// registered hook — including BrainPack itself.  Without the brain.* guard
// in on_dispatch, the hook fires a second fold.reduce, making total_events
// == 2.  This test replicates that exact sequence so the test FAILS if the
// guard is absent.
#[tokio::test]
async fn test_357_feedback_no_double_count() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    // Step 1: handle_feedback path — folds once, total_events becomes 1.
    pack.dispatch(
        "brain.feedback",
        json!({"target_id": target, "signal": "useful"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    assert_eq!(
        pack.snapshot().balanced_recall.total_events,
        1,
        "#357 pre-hook: handle_feedback must fold exactly once"
    );

    // Step 2: simulate the registry post-dispatch hook call with a brain.*
    // verb.  This is exactly what VerbRegistry::dispatch does after a
    // successful handler return.  The guard in on_dispatch must return
    // early here — without it, fold.reduce fires again → total_events = 2.
    let hook_event = {
        use khive_types::{EventKind, SubstrateKind};
        khive_storage::event::Event::new(
            "local",
            "brain.feedback",
            EventKind::FeedbackExplicit,
            SubstrateKind::Event,
            "brain",
        )
    };
    let hook_view = khive_runtime::EventView {
        event: hook_event,
        observations: Vec::new(),
    };
    pack.on_dispatch(&hook_view).await;

    assert_eq!(
        pack.snapshot().balanced_recall.total_events,
        1,
        "#357: total_events must remain 1 after on_dispatch(brain.feedback); \
         guard absent if this reads 2"
    );
}

// brain.reset must restore domain-informed priors, not Beta(1,1).
//
// This test now exercises the full
// production path — handle_reset → reset_posteriors → sync helper — and
// verifies that ALL three profile record fields (total_events,
// exploration_epoch, state_snapshot) reflect the restored priors.
//
// It also creates a stale record via hook-only updates (bypassing
// handle_feedback) before the reset, so the desync is real and not
// incidentally corrected by the feedback path.
#[tokio::test]
async fn test_295_reset_restores_domain_priors_not_uniform() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    // Step 0: trigger namespace load before any direct state mutations or hook fires,
    // so ensure_loaded does not reset state after hook events are accumulated.
    // (BRAIN-AUD-001 fix: ensure_loaded resets to a fresh state for any unloaded namespace.)
    pack.dispatch("brain.profiles", json!({}), &registry, &token)
        .await
        .expect("trigger namespace load");

    // Step 1: accumulate state via hook-only updates (no handle_feedback).
    // This simulates the common case where brain observes external pack
    // events rather than explicit feedback calls.
    let hook_event = |verb: &str| {
        use khive_types::{EventKind, SubstrateKind};
        let mut e = khive_storage::event::Event::new(
            "local",
            verb,
            EventKind::Audit,
            SubstrateKind::Event,
            "kg",
        );
        e.outcome = khive_types::EventOutcome::Success;
        e
    };

    // Fire 4 hook events for a non-brain verb (simulates external recall/search).
    for _ in 0..4 {
        let view = khive_runtime::EventView {
            event: hook_event("search"),
            observations: Vec::new(),
        };
        pack.on_dispatch(&view).await;
    }

    // Step 2: also call handle_feedback directly to move salience away from prior.
    // C4: create a real entity so target_id validation passes.
    let target = create_test_entity(&rt, &token).await;
    for _ in 0..5 {
        pack.dispatch(
            "brain.feedback",
            json!({"target_id": target, "signal": "useful"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    }

    // Verify state before reset: posteriors have moved, total_events > 0.
    let before = pack.snapshot();
    assert!(
        before.balanced_recall.salience.alpha() > 2.0,
        "salience.alpha() must have grown past prior after useful feedback"
    );
    assert!(
        before.balanced_recall.total_events >= 9,
        "expected at least 9 total events (4 hook + 5 feedback), got {}",
        before.balanced_recall.total_events
    );
    // Verify record was kept in sync before reset (both paths called sync helper).
    let pre_reset_record = pack
        .dispatch(
            "brain.profile",
            json!({"id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(
        pre_reset_record["total_events"].as_u64().unwrap(),
        before.balanced_recall.total_events,
        "#295 pre-reset: profile record total_events out of sync before reset"
    );

    // Step 3: call handle_reset via the production path (dispatch → handle_reset).
    let reset_result = pack
        .dispatch(
            "brain.reset",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(reset_result["reset"], json!(true));

    // Verify exploration_epoch incremented (reset_posteriors contract).
    let epoch_after = reset_result["exploration_epoch"].as_u64().unwrap();
    assert!(
        epoch_after > 0,
        "#295: exploration_epoch must increment after reset"
    );

    // Step 4: after reset, posteriors must be domain-informed priors — NOT Beta(1,1).
    let after = pack.snapshot();

    // salience prior = Beta(2,8)
    assert!(
        (after.balanced_recall.salience.alpha() - 2.0).abs() < 1e-12,
        "#295: salience.alpha() must be 2.0 after reset, got {}",
        after.balanced_recall.salience.alpha()
    );
    assert!(
        (after.balanced_recall.salience.beta() - 8.0).abs() < 1e-12,
        "#295: salience.beta() must be 8.0 after reset, got {}",
        after.balanced_recall.salience.beta()
    );

    // temporal prior = Beta(1,9)
    assert!(
        (after.balanced_recall.temporal.alpha() - 1.0).abs() < 1e-12,
        "#295: temporal.alpha() must be 1.0 after reset, got {}",
        after.balanced_recall.temporal.alpha()
    );
    assert!(
        (after.balanced_recall.temporal.beta() - 9.0).abs() < 1e-12,
        "#295: temporal.beta() must be 9.0 after reset, got {}",
        after.balanced_recall.temporal.beta()
    );

    // relevance prior = Beta(7,3)
    assert!(
        (after.balanced_recall.relevance.alpha() - 7.0).abs() < 1e-12,
        "#295: relevance.alpha() must be 7.0 after reset"
    );

    // Step 5: brain.profile must reflect the reset state — ALL three fields.
    // This pins the sync_balanced_recall_record call inside handle_reset.
    // Removing that call would cause this assertion to fail.
    let record = pack
        .dispatch(
            "brain.profile",
            json!({"id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();

    // total_events: after reset the state is a fresh BalancedRecallState
    // (total_events = 0), so the record must reflect that.
    let record_total = record["total_events"].as_u64().unwrap();
    assert_eq!(
        record_total, after.balanced_recall.total_events,
        "#295: profile record total_events ({record_total}) must match \
         live state ({}) after reset",
        after.balanced_recall.total_events
    );

    // exploration_epoch: record must match the live state.
    let record_epoch = record["exploration_epoch"].as_u64().unwrap();
    assert_eq!(
        record_epoch, epoch_after,
        "#295: profile record exploration_epoch ({record_epoch}) must match \
         reset result ({epoch_after})"
    );

    // state_snapshot: salience.alpha() must be the prior value.
    let snap = &record["state_snapshot"];
    let sal_alpha = snap["salience"]["alpha"].as_f64().unwrap();
    assert!(
        (sal_alpha - 2.0).abs() < 1e-12,
        "#295: brain.profile state_snapshot salience.alpha() must be 2.0 after reset, \
         got {sal_alpha}"
    );
}

// brain.reset must reject unknown kwargs (deny_unknown_fields).
#[tokio::test]
async fn brain_reset_rejects_unknown_kwargs() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let err = pack
        .dispatch(
            "brain.reset",
            json!({"unknownkw": "oops"}),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::InvalidInput(_)),
        "brain.reset with unknown kwargs must return InvalidInput, got: {err:?}"
    );
    if let RuntimeError::InvalidInput(msg) = &err {
        assert!(
            msg.contains("brain.reset"),
            "error message must mention brain.reset, got: {msg}"
        );
    }
}

// brain.reset with an empty params object must still succeed.
#[tokio::test]
async fn brain_reset_accepts_empty_params() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let result = pack
        .dispatch("brain.reset", json!({}), &registry, &token)
        .await
        .expect("brain.reset() must succeed with empty params");
    assert_eq!(result["reset"], json!(true));
}

// (regression — real dispatch path): temporal posterior must update
// when a recall hits via the on_dispatch hook carrying real hit/latency.
//
// This test exercises the production wiring added in the P12 fix:
// the runtime now embeds duration_us + target_id in the hook event for
// "recall" verbs.  Simulates that by constructing the hook event the way
// the runtime now would, then verifies temporal.alpha() increments.
#[tokio::test]
async fn test_355_posteriors_update_after_dispatch_via_hook() {
    let (pack, _rt) = make_pack();
    pack.activate_namespace_for_test("local");
    let before = pack.snapshot();
    let tmp_alpha_before = before.balanced_recall.temporal.alpha();
    let tmp_beta_before = before.balanced_recall.temporal.beta();

    // Simulate the runtime hook event for a fast recall hit:
    // duration_us ≤ 50_000 (fast) and target_id is present (hit).
    let target_id = uuid::Uuid::new_v4();
    let fast_hit_event = {
        use khive_types::{EventKind, SubstrateKind};
        let mut e = khive_storage::event::Event::new(
            "local",
            "recall",
            EventKind::Audit,
            SubstrateKind::Event,
            "memory",
        );
        e.outcome = khive_types::EventOutcome::Success;
        e.target_id = Some(target_id);
        e.duration_us = 10_000; // 10 ms — fast hit
        e
    };
    let view = khive_runtime::EventView {
        event: fast_hit_event,
        observations: Vec::new(),
    };
    pack.on_dispatch(&view).await;

    let after_fast = pack.snapshot();
    assert!(
        (after_fast.balanced_recall.temporal.alpha() - (tmp_alpha_before + 1.0)).abs() < 1e-12,
        "#355: fast recall hit must increment temporal.alpha() via hook: expected {}, got {}",
        tmp_alpha_before + 1.0,
        after_fast.balanced_recall.temporal.alpha()
    );
    assert!(
        (after_fast.balanced_recall.temporal.beta() - tmp_beta_before).abs() < 1e-12,
        "#355: fast hit must NOT increment temporal.beta()"
    );

    // Simulate a slow recall hit (duration_us > 50_000) → temporal failure.
    let slow_hit_event = {
        use khive_types::{EventKind, SubstrateKind};
        let mut e = khive_storage::event::Event::new(
            "local",
            "recall",
            EventKind::Audit,
            SubstrateKind::Event,
            "memory",
        );
        e.outcome = khive_types::EventOutcome::Success;
        e.target_id = Some(target_id);
        e.duration_us = 100_000; // 100 ms — slow
        e
    };
    let view2 = khive_runtime::EventView {
        event: slow_hit_event,
        observations: Vec::new(),
    };
    pack.on_dispatch(&view2).await;

    let after_slow = pack.snapshot();
    assert!(
        (after_slow.balanced_recall.temporal.beta() - (tmp_beta_before + 1.0)).abs() < 1e-12,
        "#355: slow recall hit must increment temporal.beta() via hook: expected {}, got {}",
        tmp_beta_before + 1.0,
        after_slow.balanced_recall.temporal.beta()
    );

    // Simulate a recall miss (no target_id) → temporal failure.
    let miss_event = {
        use khive_types::{EventKind, SubstrateKind};
        let mut e = khive_storage::event::Event::new(
            "local",
            "recall",
            EventKind::Audit,
            SubstrateKind::Event,
            "memory",
        );
        e.outcome = khive_types::EventOutcome::Success;
        // target_id = None → RecallMiss
        e
    };
    let view3 = khive_runtime::EventView {
        event: miss_event,
        observations: Vec::new(),
    };
    pack.on_dispatch(&view3).await;

    let after_miss = pack.snapshot();
    assert!(
        (after_miss.balanced_recall.temporal.beta() - (tmp_beta_before + 2.0)).abs() < 1e-12,
        "#355: recall miss must further increment temporal.beta(): expected {}, got {}",
        tmp_beta_before + 2.0,
        after_miss.balanced_recall.temporal.beta()
    );
}

/// #1475/#1486: automatic recall evidence follows the profile that actually
/// served, while explicit failed-read attribution never falls back to default.
#[tokio::test]
async fn recall_hook_routes_known_profile_and_drops_unattributed_signal() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    pack.dispatch(
        "brain.create_profile",
        json!({"name": "hook-custom-v1", "consumer_kind": "recall"}),
        &registry,
        &token,
    )
    .await
    .expect("create custom profile");

    let before = pack.snapshot();
    let default_before = before.balanced_recall.clone();
    let custom_before = before.profile_states["hook-custom-v1"].clone();

    let target_id = uuid::Uuid::new_v4();
    let mut attributed_event = khive_storage::event::Event::new(
        "local",
        "memory.recall",
        khive_types::EventKind::Audit,
        khive_types::SubstrateKind::Event,
        "memory",
    );
    attributed_event.target_id = Some(target_id);
    attributed_event.duration_us = 10_000;
    attributed_event.payload = json!({
        "served_by_profile_id": "hook-custom-v1",
        "serve_attribution": "profile"
    });
    pack.on_dispatch(&khive_runtime::EventView {
        event: attributed_event,
        observations: Vec::new(),
    })
    .await;

    let after_attributed = pack.snapshot();
    assert_eq!(
        after_attributed.balanced_recall, default_before,
        "known custom-profile recall must not credit balanced-recall-v1"
    );
    let custom_after = after_attributed.profile_states["hook-custom-v1"].clone();
    assert_eq!(custom_after.total_events, custom_before.total_events + 1);
    assert_eq!(
        custom_after.relevance.alpha(),
        custom_before.relevance.alpha() + 1.0
    );

    let mut unattributed_event = khive_storage::event::Event::new(
        "local",
        "memory.recall",
        khive_types::EventKind::Audit,
        khive_types::SubstrateKind::Event,
        "memory",
    );
    unattributed_event.target_id = Some(uuid::Uuid::new_v4());
    unattributed_event.duration_us = 10_000;
    unattributed_event.payload = json!({"serve_attribution": "unattributed"});
    pack.on_dispatch(&khive_runtime::EventView {
        event: unattributed_event,
        observations: Vec::new(),
    })
    .await;

    let after_unattributed = pack.snapshot();
    assert_eq!(
        after_unattributed.balanced_recall, default_before,
        "failed-read recall must not fall back to balanced-recall-v1"
    );
    assert_eq!(
        after_unattributed.profile_states["hook-custom-v1"], custom_after,
        "failed-read recall must not credit the selected custom profile"
    );
}

/// #3314: a successful memory recall must return from the runtime hook even
/// while a long brain handler owns the namespace gate. The deferred signal
/// still credits the profile named by the serving result after the gate opens.
#[tokio::test(flavor = "current_thread")]
async fn contended_brain_gate_does_not_hold_hook_caller_or_lose_attribution() {
    use std::time::Duration;

    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    pack.dispatch(
        "brain.create_profile",
        json!({"name": "deferred-profile-v1", "consumer_kind": "recall"}),
        &registry,
        &token,
    )
    .await
    .expect("create serving profile");

    let before = pack.snapshot();
    let default_events = before.balanced_recall.total_events;
    let served_events = before.profile_states["deferred-profile-v1"].total_events;
    let mut event = khive_storage::event::Event::new(
        "local",
        "memory.recall",
        khive_types::EventKind::Audit,
        khive_types::SubstrateKind::Event,
        "memory",
    );
    event.target_id = Some(uuid::Uuid::new_v4());
    event.payload = json!({
        "served_by_profile_id": "deferred-profile-v1",
        "serve_attribution": "profile"
    });
    let view = khive_runtime::EventView {
        event,
        observations: Vec::new(),
    };

    let gate = pack.dispatch_gate.lock().await;
    let unrelated = khive_runtime::EventView {
        event: khive_storage::event::Event::new(
            "local",
            "comm.send",
            khive_types::EventKind::Audit,
            khive_types::SubstrateKind::Event,
            "comm",
        ),
        observations: Vec::new(),
    };
    tokio::time::timeout(Duration::from_millis(200), pack.on_dispatch(&unrelated))
        .await
        .expect("an irrelevant verb must not touch the brain gate");
    tokio::time::timeout(Duration::from_millis(200), pack.on_dispatch(&view))
        .await
        .expect("hook must hand off without awaiting a brain handler");
    assert_eq!(
        pack.snapshot().profile_states["deferred-profile-v1"].total_events,
        served_events,
        "deferred evidence must not apply outside the namespace gate"
    );
    drop(gate);

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if pack.snapshot().profile_states["deferred-profile-v1"].total_events
                == served_events + 1
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("deferred evidence must drain after the gate opens");
    assert_eq!(pack.snapshot().balanced_recall.total_events, default_events);
}

/// #3316: ordinary irrelevant dispatches do not establish a cold queue;
/// useful cold signals have a ceiling and expose overflow in diagnostics.
#[tokio::test]
async fn cold_hook_overflow_is_visible_without_loading_namespace() {
    let (pack, _) = make_pack();
    let irrelevant = khive_runtime::EventView {
        event: khive_storage::event::Event::new(
            "never-loaded",
            "comm.send",
            khive_types::EventKind::Audit,
            khive_types::SubstrateKind::Event,
            "comm",
        ),
        observations: Vec::new(),
    };
    pack.on_dispatch(&irrelevant).await;
    assert!(pack
        .persistence
        .lock()
        .unwrap()
        .drain_pending_signals("never-loaded")
        .is_empty());

    let recall_miss = khive_runtime::EventView {
        event: khive_storage::event::Event::new(
            "never-loaded",
            "memory.recall",
            khive_types::EventKind::Audit,
            khive_types::SubstrateKind::Event,
            "memory",
        ),
        observations: Vec::new(),
    };
    for _ in 0..=crate::persist::MAX_PENDING_HOOK_SIGNALS_PER_NAMESPACE {
        pack.on_dispatch(&recall_miss).await;
    }
    let state = pack.handle_state(json!({})).await.unwrap();
    assert_eq!(state["dispatch_counters"]["cold_hook_signals_dropped"], 1);
    assert_eq!(
        pack.persistence
            .lock()
            .unwrap()
            .drain_pending_signals("never-loaded")
            .len(),
        crate::persist::MAX_PENDING_HOOK_SIGNALS_PER_NAMESPACE
    );
    assert!(
        !pack.persistence.lock().unwrap().is_loaded("never-loaded"),
        "hook observations cannot skip the first durable namespace load"
    );
}

include!("tests/binding_lifecycle_tests.rs");

// C4: brain.feedback must reject nonexistent target_id.
#[tokio::test]
async fn w4_c4_feedback_rejects_nonexistent_target() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    let err = pack
        .dispatch(
            "brain.feedback",
            json!({"target_id": "00000000-0000-0000-0000-000000000000", "signal": "useful"}),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::NotFound(_)),
        "C4: feedback with nonexistent target_id must return NotFound, got {err:?}"
    );
}

// C4: brain.feedback must reject nonexistent served_by_profile_id.
#[tokio::test]
async fn w4_c4_feedback_rejects_nonexistent_profile() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    // Create a real entity for the valid target_id.
    let target = create_test_entity(&rt, &token).await;

    let err = pack
        .dispatch(
            "brain.feedback",
            json!({"target_id": target, "signal": "useful", "served_by_profile_id": "fake-profile-xyz"}),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, RuntimeError::NotFound(_)),
        "C4: feedback with nonexistent served_by_profile_id must return NotFound, got {err:?}"
    );
}

// C4: brain.feedback with valid target and known profile must succeed.
#[tokio::test]
async fn w4_c4_feedback_accepts_valid_target_and_profile() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    let target = create_test_entity(&rt, &token).await;

    let result = pack
        .dispatch(
            "brain.feedback",
            json!({"target_id": target, "signal": "useful", "served_by_profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(result["emitted"], json!(true));
    assert_eq!(result["signal"], json!("useful"));
}

#[tokio::test]
async fn feedback_success_counts_one_event_row_for_each_append_path() {
    for signal in ["explicit_positive", "correction", "implicit_positive"] {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local()).unwrap();
        let target = create_test_entity(&rt, &token).await;
        let usage = khive_storage::usage::UsageContext::new();

        let result = khive_storage::usage::scope(
            usage.clone(),
            pack.dispatch(
                "brain.feedback",
                json!({"target_id": target, "signal": signal}),
                &registry,
                &token,
            ),
        )
        .await
        .expect("feedback append must commit");

        assert_eq!(result["emitted"], json!(true));
        assert_eq!(
            usage.snapshot()["event_rows"],
            json!(1u64),
            "signal {signal:?} must count its one committed public event row"
        );
    }
}

/// Regression (#831): a note-target `brain.feedback` signal must resolve via
/// `observed_as_signal` (previously unreachable — the decoder hard-coded
/// `ReferentKind::Entity`). See crates/khive-pack-brain/docs/dispatch-namespace-isolation.md#feedback_note_target_resolves_through_observed_as_signal.
#[tokio::test]
async fn feedback_note_target_resolves_through_observed_as_signal() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    let note = rt
        .create_note(
            &token,
            "observation",
            None,
            "a real stored note used as a feedback target",
            None,
            None,
            vec![],
        )
        .await
        .expect("create test note");

    let result = pack
        .dispatch(
            "brain.feedback",
            json!({"target_id": note.id.to_string(), "signal": "useful", "served_by_profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    assert_eq!(result["emitted"], json!(true));

    let rows = rt
        .query(&token, "MATCH (ev)-[:observed_as_signal]->(t) RETURN t")
        .await
        .unwrap();

    let note_id_str = note.id.to_string();
    let found = rows.iter().any(|row| {
        row.columns.iter().any(|col| match &col.value {
            khive_storage::types::SqlValue::Text(s) => s == &note_id_str,
            khive_storage::types::SqlValue::Uuid(u) => u.to_string() == note_id_str,
            _ => false,
        })
    });
    assert!(
        found,
        "note feedback target must resolve through observed_as_signal; rows: {:?}",
        rows
    );
}

include!("tests/profile_creation_and_resolution_tests.rs");

// brain.feedback rejects archived served_by_profile_id.
#[tokio::test]
async fn r2_feedback_rejects_archived_served_by_profile() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    create_active_lifecycle_profile(&pack, &registry, &token).await;

    let target = create_test_entity(&rt, &token).await;

    // Archive the custom profile.
    pack.dispatch(
        "brain.deactivate",
        json!({"profile_id": LIFECYCLE_PROFILE_ID}),
        &registry,
        &token,
    )
    .await
    .unwrap();
    pack.dispatch(
        "brain.archive",
        json!({"profile_id": LIFECYCLE_PROFILE_ID}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    let err = pack
        .dispatch(
            "brain.feedback",
            json!({
                "target_id": target,
                "signal": "useful",
                "served_by_profile_id": LIFECYCLE_PROFILE_ID
            }),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    if let RuntimeError::InvalidInput(msg) = &err {
        assert!(
            msg.contains("archived"),
            "feedback to archived profile must mention 'archived'; got: {msg}"
        );
    } else {
        panic!("feedback to archived served_by_profile_id must return InvalidInput, got {err:?}");
    }
}

include!("tests/profile_creation_and_reset_tests.rs");

// Feedback routes to the user-created profile's own state.
#[tokio::test]
async fn r2_user_profile_feedback_routes_to_profile_state() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    let target = create_test_entity(&rt, &token).await;

    pack.dispatch(
        "brain.create_profile",
        json!({"name": "custom-v1", "consumer_kind": "recall"}),
        &registry,
        &token,
    )
    .await
    .unwrap();
    pack.dispatch(
        "brain.activate",
        json!({"profile_id": "custom-v1"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    let before = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": "custom-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    let events_before = before["total_events"].as_u64().unwrap();

    pack.dispatch(
        "brain.feedback",
        json!({"target_id": target, "signal": "useful", "served_by_profile_id": "custom-v1"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    let after = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": "custom-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    let events_after = after["total_events"].as_u64().unwrap();
    assert!(
        events_after > events_before,
        "feedback routed to custom profile must increment its total_events; before={events_before} after={events_after}"
    );
}

// ── #697: feedback-path tier-2 binding resolution ─────────────────────────

/// Build a runtime whose minted tokens carry a configured actor identity
/// (rather than `ActorRef::anonymous()`), so actor-scoped bindings can match.
fn make_pack_with_actor(actor_id: &str) -> (BrainPack, KhiveRuntime) {
    // Mirror KhiveRuntime::memory() exactly (in-memory db, NO embedding model)
    // plus a configured actor. `..RuntimeConfig::default()` is a CI trap: the
    // Default impl resolves embedding_model to a real on-disk model, which is
    // absent on CI runners and fails entity creation with ModelInitialization.
    let rt = KhiveRuntime::new(khive_runtime::RuntimeConfig {
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_source: Default::default(),
        wal_ceiling_env_raw: None,
        web: Default::default(),
        telemetry: Default::default(),
        mounts: Vec::new(),
        git_write: Default::default(),
        brain: Default::default(),
        exec: Default::default(),
        display_timezone: khive_runtime::config::resolve_default_display_timezone(),
        events_split: None,
        db_path: None,
        blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        gate: std::sync::Arc::new(khive_runtime::AllowAllGate),
        packs: vec!["kg".to_string()],
        backend_id: khive_runtime::BackendId::main(),
        brain_profile: None,
        visible_namespaces: vec![],
        allowed_outbound_namespaces: vec![],
        actor_id: Some(actor_id.to_string()),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime with actor");
    let pack = BrainPack::new(rt.clone());
    (pack, rt)
}

/// #697 (a): feedback without an explicit `served_by_profile_id` but with a
/// matching actor binding must attribute to the bound profile — not
/// balanced-recall-v1 — and must leave the default profile's posteriors untouched.
#[tokio::test]
async fn feedback_697_unspecified_resolves_via_actor_binding() {
    let (pack, rt) = make_pack_with_actor("test-actor");
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    assert_eq!(
        token.actor().id,
        "test-actor",
        "test setup: token must carry the configured actor"
    );

    let target = create_test_entity(&rt, &token).await;

    pack.dispatch(
        "brain.create_profile",
        json!({"name": "test-bound-v1", "consumer_kind": "recall"}),
        &registry,
        &token,
    )
    .await
    .unwrap();
    pack.dispatch(
        "brain.activate",
        json!({"profile_id": "test-bound-v1"}),
        &registry,
        &token,
    )
    .await
    .unwrap();
    // Bind by actor only (namespace left as the "*" wildcard) — a
    // namespace-only resolution (pre-#697) can never reach this binding.
    pack.dispatch(
        "brain.bind",
        json!({"actor": "test-actor", "profile_id": "test-bound-v1", "consumer_kind": "recall"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    let default_before = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap()["total_events"]
        .as_u64()
        .unwrap();

    // No served_by_profile_id supplied.
    pack.dispatch(
        "brain.feedback",
        json!({"target_id": target, "signal": "useful"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    let bound_after = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": "test-bound-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap()["total_events"]
        .as_u64()
        .unwrap();
    assert_eq!(
        bound_after, 1,
        "unspecified feedback with a matching actor binding must land on the bound profile"
    );

    let default_after = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap()["total_events"]
        .as_u64()
        .unwrap();
    assert_eq!(
        default_after, default_before,
        "the default profile's posteriors must be untouched when a binding resolves the event elsewhere"
    );
}

/// #1486: explicit failed-read attribution is not the same as omission. The
/// selected recall result carries the marker through auto_feedback, which records
/// a forced-zero event without invoking the otherwise-matching actor binding.
#[tokio::test]
async fn auto_feedback_failed_read_marker_does_not_fall_back_to_binding() {
    let (pack, rt) = make_pack_with_actor("failed-read-actor");
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    pack.dispatch(
        "brain.create_profile",
        json!({"name": "failed-read-bound-v1", "consumer_kind": "recall"}),
        &registry,
        &token,
    )
    .await
    .expect("create bound profile");
    pack.dispatch(
        "brain.bind",
        json!({
            "actor": "failed-read-actor",
            "profile_id": "failed-read-bound-v1",
            "consumer_kind": "recall"
        }),
        &registry,
        &token,
    )
    .await
    .expect("bind profile");

    let before = pack.snapshot();
    let default_before = before.balanced_recall.clone();
    let bound_before = before.profile_states["failed-read-bound-v1"].clone();

    let result = pack
        .dispatch(
            "brain.auto_feedback",
            json!({
                "query": "failed profile read",
                "target_id": target,
                "signal": "implicit_positive",
                "results": [{
                    "id": target,
                    "serve_attribution": "unattributed"
                }]
            }),
            &registry,
            &token,
        )
        .await
        .expect("unattributed auto feedback is recorded at zero weight");

    assert_eq!(result["emitted"], json!(true));
    assert_eq!(result["serve_attribution"], json!("unattributed"));
    assert_eq!(result["served_by_profile_id"], Value::Null);

    let after = pack.snapshot();
    assert_eq!(after.balanced_recall, default_before);
    assert_eq!(
        after.profile_states["failed-read-bound-v1"], bound_before,
        "failed-read marker must bypass the matching binding entirely"
    );

    let store = rt.events(&token).expect("event store");
    let page = store
        .query_events(
            khive_storage::event::EventFilter {
                kinds: vec![khive_types::EventKind::FeedbackExplicit],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .expect("query feedback event");
    let event = page.items.first().expect("forced-zero feedback event");
    assert_eq!(event.payload["serve_attribution"], json!("unattributed"));
    assert_eq!(event.payload["served_by_profile_id"], Value::Null);
    assert_eq!(event.payload["gate"]["effective_weight"], json!(0.0));
    assert_eq!(event.payload["gate"]["forced_zero_weight"], json!(true));
}

#[tokio::test]
async fn auto_feedback_top_level_attribution_pair_wins_over_result_metadata() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    pack.dispatch(
        "brain.create_profile",
        json!({"name": "top-level-profile-v1", "consumer_kind": "recall"}),
        &registry,
        &token,
    )
    .await
    .expect("create explicit profile");

    let before = pack.snapshot();
    let default_before = before.balanced_recall.clone();
    let custom_before = before.profile_states["top-level-profile-v1"].total_events;

    let result = pack
        .dispatch(
            "brain.auto_feedback",
            json!({
                "query": "explicit top-level attribution",
                "target_id": target,
                "signal": "implicit_positive",
                "served_by_profile_id": "top-level-profile-v1",
                "results": [{
                    "id": target,
                    "serve_attribution": "unattributed"
                }]
            }),
            &registry,
            &token,
        )
        .await
        .expect("top-level legacy profile id must override result attribution");

    assert_eq!(
        result["served_by_profile_id"],
        json!("top-level-profile-v1")
    );
    assert_eq!(result["serve_attribution"], json!("profile"));
    let after = pack.snapshot();
    assert_eq!(after.balanced_recall, default_before);
    assert_eq!(
        after.profile_states["top-level-profile-v1"].total_events,
        custom_before + 1
    );
}

/// #1016: every persisted feedback event payload must carry the RESOLVED
/// `served_by_profile_id` plus the `profile_resolution` marker — across all
/// three resolution tiers AND the gated implicit append path (which builds
/// its event inside the fold-gate atomic unit from a cloned `base_data`).
#[tokio::test]
async fn feedback_1016_event_payload_stamps_resolved_profile_all_tiers() {
    use khive_storage::event::EventFilter;
    use khive_storage::types::PageRequest;

    let (pack, rt) = make_pack_with_actor("test-actor");
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    // Tier 3 (default): no explicit value, no binding — ordinary append path.
    pack.dispatch(
        "brain.feedback",
        json!({"target_id": target, "signal": "useful"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    // Tier 3, gated implicit: same resolution, fold-gate append closure.
    pack.dispatch(
        "brain.feedback",
        json!({"target_id": target, "signal": "implicit_positive"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    // Tier 2 (binding): bound profile resolves when the caller omits the id.
    for (verb, params) in [
        (
            "brain.create_profile",
            json!({"name": "test-bound-v1", "consumer_kind": "recall"}),
        ),
        ("brain.activate", json!({"profile_id": "test-bound-v1"})),
        (
            "brain.bind",
            json!({"actor": "test-actor", "profile_id": "test-bound-v1", "consumer_kind": "recall"}),
        ),
    ] {
        pack.dispatch(verb, params, &registry, &token)
            .await
            .unwrap();
    }
    pack.dispatch(
        "brain.feedback",
        json!({"target_id": target, "signal": "useful"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    // Tier 1 (explicit): caller-supplied id wins over the binding.
    pack.dispatch(
        "brain.feedback",
        json!({
            "target_id": target,
            "signal": "useful",
            "served_by_profile_id": "balanced-recall-v1"
        }),
        &registry,
        &token,
    )
    .await
    .unwrap();

    let events = rt
        .events(&token)
        .expect("event store")
        .query_events(
            EventFilter::default(),
            PageRequest {
                offset: 0,
                limit: 1000,
            },
        )
        .await
        .expect("query_events");
    let mut stamps: Vec<(String, String, String)> = events
        .items
        .iter()
        .filter(|e| e.verb == "brain.feedback")
        .map(|e| {
            (
                e.payload["signal"].as_str().unwrap_or("").to_string(),
                e.payload["served_by_profile_id"]
                    .as_str()
                    .unwrap_or("<missing>")
                    .to_string(),
                e.payload["profile_resolution"]
                    .as_str()
                    .unwrap_or("<missing>")
                    .to_string(),
            )
        })
        .collect();
    stamps.sort();

    let mut expected = vec![
        (
            "useful".to_string(),
            "balanced-recall-v1".to_string(),
            "default".to_string(),
        ),
        (
            "implicit_positive".to_string(),
            "balanced-recall-v1".to_string(),
            "default".to_string(),
        ),
        (
            "useful".to_string(),
            "test-bound-v1".to_string(),
            "binding".to_string(),
        ),
        (
            "useful".to_string(),
            "balanced-recall-v1".to_string(),
            "explicit".to_string(),
        ),
    ];
    expected.sort();

    assert_eq!(
        stamps, expected,
        "#1016: all four feedback events must persist the resolved profile and its resolution marker"
    );
}

/// Systemic-fix regression: an ANONYMOUS caller must not match an explicit
/// `actor="local"` binding. `ActorRef::anonymous()` carries `id: "local"`
/// (`khive-gate/src/actor.rs`); before the `binding_id()` fix,
/// `resolve_effective_feedback_profile` threaded `token.actor().id`
/// unconditionally, so an anonymous token could match a binding a
/// pre-actor-aware `None` never could.
#[tokio::test]
async fn feedback_anonymous_caller_does_not_match_explicit_actor_local_binding() {
    // No actor_id configured — `rt.authorize` mints the anonymous actor.
    let (pack, rt) = make_anonymous_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    assert!(
        token.actor().is_anonymous(),
        "test setup: token must carry the anonymous actor"
    );

    let target = create_test_entity(&rt, &token).await;

    pack.dispatch(
        "brain.create_profile",
        json!({"name": "anon-local-v1", "consumer_kind": "recall"}),
        &registry,
        &token,
    )
    .await
    .unwrap();
    pack.dispatch(
        "brain.activate",
        json!({"profile_id": "anon-local-v1"}),
        &registry,
        &token,
    )
    .await
    .unwrap();
    // Bind explicitly to actor="local" — the exact id anonymous tokens carry.
    pack.dispatch(
        "brain.bind",
        json!({"actor": "local", "profile_id": "anon-local-v1", "consumer_kind": "recall"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    let default_before = pack.snapshot().balanced_recall.total_events;

    // An actor-local binding cannot authorize anonymous explicit feedback.
    let err = pack
        .dispatch(
            "brain.feedback",
            json!({"target_id": target, "signal": "useful"}),
            &registry,
            &token,
        )
        .await
        .expect_err("anonymous explicit feedback must refuse before profile routing");
    assert_anonymous_feedback_refusal(err);

    let bound_events = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": "anon-local-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap()["total_events"]
        .as_u64()
        .unwrap();
    assert_eq!(
        bound_events, 0,
        "anonymous caller must NOT match the actor=\"local\" binding"
    );

    let default_events = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap()["total_events"]
        .as_u64()
        .unwrap();
    assert_eq!(
        default_events, default_before,
        "refusal must leave the default profile unchanged"
    );
}

/// #697 (b): feedback with neither an explicit profile nor any matching
/// binding still defaults to balanced-recall-v1 — existing behavior preserved.
#[tokio::test]
async fn feedback_697_unspecified_no_binding_falls_back_to_default() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    let target = create_test_entity(&rt, &token).await;

    let default_before = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap()["total_events"]
        .as_u64()
        .unwrap();

    pack.dispatch(
        "brain.feedback",
        json!({"target_id": target, "signal": "useful"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    let default_after = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap()["total_events"]
        .as_u64()
        .unwrap();
    assert_eq!(
        default_after,
        default_before + 1,
        "feedback with no binding and no explicit profile must still credit balanced-recall-v1"
    );
}

/// #1851: unknown IDs (including bare roles) use the caller's recall binding,
/// and both feedback entry points persist the resolved attribution.
#[tokio::test]
async fn feedback_1851_unknown_profile_uses_recall_binding() {
    use khive_storage::event::EventFilter;
    use khive_storage::types::PageRequest;

    for (verb, requested, selected_metadata) in [
        ("brain.feedback", "researcher", false),
        ("brain.auto_feedback", "unknown-profile-v1", false),
        ("brain.auto_feedback", "researcher", true),
    ] {
        let (pack, rt) = make_pack_with_actor("bound-feedback-actor");
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local()).unwrap();
        let target = create_test_entity(&rt, &token).await;
        for (setup_verb, params) in [
            (
                "brain.create_profile",
                json!({"name": "actual-recall-v1", "consumer_kind": "recall"}),
            ),
            ("brain.activate", json!({"profile_id": "actual-recall-v1"})),
            (
                "brain.bind",
                json!({"actor": "bound-feedback-actor", "profile_id": "actual-recall-v1", "consumer_kind": "recall"}),
            ),
        ] {
            pack.dispatch(setup_verb, params, &registry, &token)
                .await
                .unwrap();
        }
        let before = pack.snapshot();
        let mut params = json!({"target_id": target, "signal": "useful"});
        if selected_metadata {
            params["results"] = json!([{"id": target, "served_by_profile_id": requested}]);
        } else {
            params["served_by_profile_id"] = json!(requested);
            params["results"] = json!([{"id": target}]);
        }
        if verb == "brain.feedback" {
            params.as_object_mut().unwrap().remove("results");
        } else {
            params["query"] = json!("profile fallback");
        }
        let result = pack
            .dispatch(verb, params, &registry, &token)
            .await
            .unwrap();
        assert_eq!(result["served_by_profile_id"], "actual-recall-v1");
        let after = pack.snapshot();
        assert_eq!(after.balanced_recall, before.balanced_recall);
        assert_eq!(after.profiles.len(), before.profiles.len());
        assert!(!after.profiles.contains_key(requested));
        assert_eq!(
            after.profile_states["actual-recall-v1"].total_events,
            before.profile_states["actual-recall-v1"].total_events + 1
        );
        let events = rt
            .events(&token)
            .unwrap()
            .query_events(
                EventFilter::default(),
                PageRequest {
                    offset: 0,
                    limit: 1000,
                },
            )
            .await
            .unwrap();
        let feedback: Vec<_> = events
            .items
            .iter()
            .filter(|e| e.verb == "brain.feedback")
            .collect();
        assert_eq!(feedback.len(), 1);
        assert_eq!(
            feedback[0].payload["served_by_profile_id"],
            "actual-recall-v1"
        );
        assert_eq!(feedback[0].payload["profile_resolution"], "binding");
    }
}

/// An unknown explicit ID without a matching binding cannot train the default.
#[tokio::test]
async fn feedback_1851_unknown_unbound_profile_refuses_without_writes() {
    use khive_storage::event::EventFilter;
    use khive_storage::types::PageRequest;

    for verb in ["brain.feedback", "brain.auto_feedback"] {
        let (pack, rt) = make_pack_with_actor("unbound-feedback-actor");
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local()).unwrap();
        let target = create_test_entity(&rt, &token).await;
        // Materialize the lazily loaded default before taking the no-write
        // baseline; otherwise its construction timestamp changes on first dispatch.
        pack.dispatch(
            "brain.profile",
            json!({"profile_id":"balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
        let before = pack.snapshot();
        let mut params =
            json!({"target_id": target, "signal": "useful", "served_by_profile_id": "researcher"});
        if verb == "brain.auto_feedback" {
            params["results"] = json!([{"id": target}]);
            params["query"] = json!("profile fallback");
        }
        let err = pack
            .dispatch(verb, params, &registry, &token)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, RuntimeError::NotFound(message) if message.contains("researcher")),
            "{err:?}"
        );
        let after = pack.snapshot();
        assert_eq!(
            serde_json::to_value(&after).unwrap(),
            serde_json::to_value(&before).unwrap()
        );
        let events = rt
            .events(&token)
            .unwrap()
            .query_events(
                EventFilter::default(),
                PageRequest {
                    offset: 0,
                    limit: 1000,
                },
            )
            .await
            .unwrap();
        assert!(!events.items.iter().any(|e| e.verb == "brain.feedback"));
    }
}

/// #697 (d): a known explicit `served_by_profile_id` always wins over a matching
/// actor binding.
#[tokio::test]
async fn feedback_697_explicit_profile_overrides_actor_binding() {
    let (pack, rt) = make_pack_with_actor("test-actor");
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    let target = create_test_entity(&rt, &token).await;

    pack.dispatch(
        "brain.create_profile",
        json!({"name": "test-bound-v2", "consumer_kind": "recall"}),
        &registry,
        &token,
    )
    .await
    .unwrap();
    pack.dispatch(
        "brain.activate",
        json!({"profile_id": "test-bound-v2"}),
        &registry,
        &token,
    )
    .await
    .unwrap();
    pack.dispatch(
        "brain.bind",
        json!({"actor": "test-actor", "profile_id": "test-bound-v2", "consumer_kind": "recall"}),
        &registry,
        &token,
    )
    .await
    .unwrap();

    // Explicit param names balanced-recall-v1 even though "test-actor" has a binding.
    pack.dispatch(
        "brain.feedback",
        json!({
            "target_id": target,
            "signal": "useful",
            "served_by_profile_id": "balanced-recall-v1",
        }),
        &registry,
        &token,
    )
    .await
    .unwrap();

    let bound_events = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": "test-bound-v2"}),
            &registry,
            &token,
        )
        .await
        .unwrap()["total_events"]
        .as_u64()
        .unwrap();
    assert_eq!(
        bound_events, 0,
        "explicit served_by_profile_id must override an actor binding, not merely supplement it"
    );

    let default_events = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap()["total_events"]
        .as_u64()
        .unwrap();
    assert_eq!(
        default_events, 1,
        "explicit served_by_profile_id must be honored"
    );
}

include!("tests/default_profile_lifecycle_tests.rs");

// Archiving balanced-recall-v1 then calling brain.feedback without
// served_by_profile_id must return InvalidInput and must NOT append an event.
#[tokio::test]
async fn r3_feedback_default_profile_archived_rejected() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    let target = create_test_entity(&rt, &token).await;

    // Older snapshots could archive the default before the lifecycle guard
    // existed. Model that state directly to keep the feedback defense test.
    pack.dispatch(
        "brain.profile",
        json!({"profile_id": "balanced-recall-v1"}),
        &registry,
        &token,
    )
    .await
    .unwrap();
    pack.state
        .lock()
        .unwrap()
        .profiles
        .get_mut("balanced-recall-v1")
        .unwrap()
        .lifecycle = khive_brain_core::ProfileLifecycle::Archived;

    // Baseline: total_events on default profile before the attempted feedback.
    // Also capture brain.events count so we catch any FeedbackExplicit row that
    // sneaks past the lifecycle check via a reordered append → fold sequence.
    let snap_before = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    let events_before = snap_before["total_events"].as_u64().unwrap_or(0);

    let log_before = pack
        .dispatch("brain.events", json!({"limit": 1000}), &registry, &token)
        .await
        .unwrap();
    let log_count_before = log_before["events"]
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0);

    // feedback without served_by_profile_id must be rejected.
    let err = pack
        .dispatch(
            "brain.feedback",
            json!({"target_id": target, "signal": "useful"}),
            &registry,
            &token,
        )
        .await
        .unwrap_err();
    match &err {
        RuntimeError::InvalidInput(msg) => {
            assert!(
                msg.contains("archived"),
                "error must mention 'archived'; got: {msg}"
            );
        }
        other => panic!("expected InvalidInput(archived), got {other:?}"),
    }

    // No state mutation: total_events must be unchanged.
    let snap_after = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    let events_after = snap_after["total_events"].as_u64().unwrap_or(0);
    assert_eq!(
        events_after, events_before,
        "archived default profile must not have events appended; before={events_before} after={events_after}"
    );

    // Defense-in-depth: also verify nothing landed in the event log itself.
    // A future reorder that appends FeedbackExplicit before the lifecycle check
    // but skips the fold would leave total_events unchanged but still write to
    // the log. This assertion catches that class.
    let log_after = pack
        .dispatch("brain.events", json!({"limit": 1000}), &registry, &token)
        .await
        .unwrap();
    let log_count_after = log_after["events"].as_array().map(|a| a.len()).unwrap_or(0);
    assert_eq!(
        log_count_after, log_count_before,
        "rejected feedback must not append a FeedbackExplicit event; before={log_count_before} after={log_count_after}"
    );
}

include!("tests/profile_identifier_tests.rs");

// Feedback event must record a non-zero duration_us.
#[tokio::test]
async fn test_289_feedback_event_records_nonzero_duration() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    let result = pack
        .dispatch(
            "brain.feedback",
            json!({"target_id": target, "signal": "useful"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    let event_id = result["event_id"].as_str().unwrap().to_string();

    let log = pack
        .dispatch("brain.events", json!({"limit": 100}), &registry, &token)
        .await
        .unwrap();
    let event = log["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"].as_str() == Some(event_id.as_str()))
        .expect("#289: feedback event must appear in brain.events");

    assert!(
        event["duration_us"].as_i64().unwrap() > 0,
        "#289: feedback event duration_us must be non-zero, got {}",
        event["duration_us"]
    );
}

// ── brain.auto_feedback ───────────────────────────────────────────────────

/// #2772: the refusal for a malformed `results` has to be actionable by a caller
/// who cannot read this crate. The accepted form is asserted in the same test as
/// the refusals so the message and the contract cannot drift apart.
#[tokio::test]
async fn issue2772_malformed_results_names_the_parameter_shape_and_an_example() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let first = create_test_entity(&rt, &token).await;
    let selected = create_test_entity(&rt, &token).await;

    // The shape a caller reaches for after reading `results[].id` out of a
    // recall response, plus the two neighbouring mistakes.
    for (label, results) in [
        ("bare id strings", json!([first.clone(), selected.clone()])),
        ("numbers", json!([1, 2])),
        (
            "an object where an array belongs",
            json!({ "id": selected }),
        ),
        ("objects with a non-string id", json!([{ "id": 7 }])),
    ] {
        let error = pack
            .dispatch(
                "brain.auto_feedback",
                json!({
                    "query": "recall calibration target",
                    "results": results,
                    "target_id": selected,
                    "signal": "useful"
                }),
                &registry,
                &token,
            )
            .await
            .expect_err(&format!("{label} must be refused"));
        let message = error.to_string();
        assert!(
            matches!(error, khive_runtime::RuntimeError::InvalidInput(_)),
            "{label}: {message}"
        );
        for required in ["`results`", "array of result objects", "string", "`id`"] {
            assert!(
                message.contains(required),
                "{label}: message must contain {required}: {message}"
            );
        }
        assert!(
            message.contains(r#"results=[{"id": "1e8807ef"}, {"id": "c3f21b90"}]"#),
            "{label}: message must carry a literal example: {message}"
        );
        // The defect was a message naming an internal type. Nothing that only
        // exists in this crate may appear in it.
        for leaked in ["AutoFeedbackResult", "struct", "Vec<", "serde"] {
            assert!(
                !message.contains(leaked),
                "{label}: message must name no Rust type, found {leaked}: {message}"
            );
        }
        assert_eq!(
            pack.snapshot().balanced_recall.total_events,
            0,
            "{label}: a refused call must write nothing"
        );
    }

    // The control: one character away from the first rejected shape, and it
    // still works. Without this arm the assertions above are satisfied by a
    // handler that refuses everything.
    let accepted = pack
        .dispatch(
            "brain.auto_feedback",
            json!({
                "query": "recall calibration target",
                "results": [{ "id": first }, { "id": selected }],
                "target_id": selected,
                "signal": "useful"
            }),
            &registry,
            &token,
        )
        .await
        .expect("the object form still succeeds");
    assert_eq!(accepted["emitted"], json!(true));
    assert_eq!(pack.snapshot().balanced_recall.total_events, 1);
}

#[tokio::test]
async fn brain_auto_feedback_aliases_resolve_full_id_and_retain_selected_attribution() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let first = create_test_entity(&rt, &token).await;
    let selected = create_test_entity(&rt, &token).await;
    let alias_record = create_test_entity(&rt, &token).await;
    let compact_alias = &alias_record[..8];
    let results = json!([
        {"id": first, "full_id": first},
        {"id": compact_alias, "full_id": selected, "serve_attribution": "unattributed"}
    ]);
    for target_id in [compact_alias, selected.as_str()] {
        let result = pack
            .dispatch(
                "brain.auto_feedback",
                json!({
                    "query": "full identity attribution",
                    "results": results,
                    "target_id": target_id,
                    "signal": "implicit_positive"
                }),
                &registry,
                &token,
            )
            .await
            .expect("either alias selects the second result and resolves its full_id");
        assert_eq!(result["target_id"], selected);
        assert_eq!(result["serve_attribution"], "unattributed");
        let event_id = result["event_id"].as_str().unwrap().parse().unwrap();
        let event = rt
            .events(&token)
            .expect("event store")
            .get_event(event_id)
            .await
            .unwrap()
            .expect("feedback event");
        assert_eq!(
            event.payload["candidate_ids"],
            json!([first, compact_alias])
        );
    }
    assert_eq!(pack.snapshot().balanced_recall.total_events, 0);
}

#[tokio::test]
async fn brain_auto_feedback_equal_aliases_on_one_result_count_once() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let selected = create_test_entity(&rt, &token).await;
    let result = pack
        .dispatch(
            "brain.auto_feedback",
            json!({
                "query": "equal aliases on one result",
                "results": [{"id": selected, "full_id": selected}],
                "target_id": selected,
                "signal": "implicit_positive"
            }),
            &registry,
            &token,
        )
        .await
        .expect("equal aliases identify one result, not two matches");
    assert_eq!(result["emitted"], true);
    assert_eq!(result["target_id"], selected);
    assert_eq!(pack.snapshot().balanced_recall.total_events, 1);
}

#[tokio::test]
async fn brain_auto_feedback_full_id_rejects_conflicts_duplicates_and_malformed_values() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;
    let other = create_test_entity(&rt, &token).await;
    // The default profile is rebuilt with a fresh `created_at` the first time a
    // dispatch loads state, so that one field moves for reasons unrelated to
    // feedback. Everything else in the snapshot must stay byte-identical.
    let snapshot_without_profile_creation_times = |pack: &BrainPack| {
        let mut value = json!(pack.snapshot());
        if let Some(profiles) = value.get_mut("profiles").and_then(Value::as_object_mut) {
            for profile in profiles.values_mut() {
                if let Some(fields) = profile.as_object_mut() {
                    fields.remove("created_at");
                }
            }
        }
        value
    };
    let before = snapshot_without_profile_creation_times(&pack);
    for (results, selected, message) in [
        (
            json!([{"id": &other[..8], "full_id": other}]),
            target.as_str(),
            "does not match any results[].id or results[].full_id",
        ),
        (
            json!([{"id": target, "full_id": other}, {"id": other, "full_id": target}]),
            target.as_str(),
            "matches more than one result",
        ),
        (
            json!([{"id": other, "full_id": target}, {"id": target}]),
            target.as_str(),
            "matches more than one result",
        ),
        (
            json!([{"id": &target[..8], "full_id": target}, {"id": &target[..8], "full_id": other}]),
            &target[..8],
            "matches more than one result",
        ),
        (
            json!([{"id": &target[..8], "full_id": target}, {"id": other, "full_id": target}]),
            target.as_str(),
            "matches more than one result",
        ),
        (
            json!([{"id": target, "full_id": target}, {"id": target, "full_id": target}]),
            target.as_str(),
            "matches more than one result",
        ),
        (
            json!([{"id": target, "full_id": "not-an-id"}]),
            "not-an-id",
            "invalid full_id",
        ),
        (
            json!([{"id": target, "full_id": "not-an-id"}]),
            target.as_str(),
            "invalid full_id",
        ),
        (
            json!([{"id": &target[..8], "full_id": "not-an-id"}]),
            &target[..8],
            "invalid full_id",
        ),
        (
            json!([{"id": target, "full_id": &target[..8]}]),
            target.as_str(),
            "invalid full_id",
        ),
        (
            json!([{"id": target, "full_id": ""}]),
            target.as_str(),
            "invalid full_id",
        ),
        (
            json!([{"id": target, "full_id": 42}]),
            target.as_str(),
            "invalid type",
        ),
    ] {
        let error = pack
            .dispatch(
                "brain.auto_feedback",
                json!({
                    "query": "invalid full identity",
                    "results": results,
                    "target_id": selected,
                    "signal": "implicit_positive"
                }),
                &registry,
                &token,
            )
            .await
            .expect_err("invalid canonical identity or ambiguous aliases must refuse");
        assert!(error.to_string().contains(message), "{error}");
        assert_eq!(
            snapshot_without_profile_creation_times(&pack),
            before,
            "{error}"
        );
    }
    assert_eq!(pack.snapshot().balanced_recall.total_events, 0);
    let events = rt
        .events(&token)
        .expect("event store")
        .query_events(
            khive_storage::event::EventFilter {
                kinds: vec![khive_types::EventKind::FeedbackExplicit],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert!(events.items.is_empty());
}

#[tokio::test]
async fn brain_auto_feedback_signal_requires_a_unique_result_target() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let candidate = create_test_entity(&rt, &token).await;
    let other = create_test_entity(&rt, &token).await;

    let missing = pack
        .dispatch(
            "brain.auto_feedback",
            json!({
                "query": "missing judged target",
                "results": [{"id": candidate}],
                "signal": "implicit_positive"
            }),
            &registry,
            &token,
        )
        .await
        .expect_err("a signal without a target must be rejected");
    assert!(missing.to_string().contains("target_id` is required"));

    let outside = pack
        .dispatch(
            "brain.auto_feedback",
            json!({
                "query": "outside candidate set",
                "results": [{"id": candidate}],
                "target_id": other,
                "signal": "implicit_positive"
            }),
            &registry,
            &token,
        )
        .await
        .expect_err("a target outside results must be rejected");
    assert!(outside
        .to_string()
        .contains("does not match any results[].id or results[].full_id"));

    let duplicate = pack
        .dispatch(
            "brain.auto_feedback",
            json!({
                "query": "ambiguous duplicate candidate",
                "results": [{"id": candidate}, {"id": candidate}],
                "target_id": candidate,
                "signal": "implicit_positive"
            }),
            &registry,
            &token,
        )
        .await
        .expect_err("a duplicated target must be rejected as ambiguous");
    assert!(duplicate
        .to_string()
        .contains("matches more than one result"));
    assert_eq!(pack.snapshot().balanced_recall.total_events, 0);
}

/// #1509 cheap half: `brain.auto_feedback` must persist the `query` it received
/// and the raw candidate ids for every result (not just the selected result),
/// instead of dropping both on emission.
#[tokio::test]
async fn brain_auto_feedback_persists_query_and_candidate_ids() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let first = create_test_entity(&rt, &token).await;
    let second = create_test_entity(&rt, &token).await;

    let result = pack
        .dispatch(
            "brain.auto_feedback",
            json!({
                "query": "recall calibration target",
                "results": [{ "id": first }, { "id": second }],
                "target_id": second,
                "signal": "implicit_positive"
            }),
            &registry,
            &token,
        )
        .await
        .expect("auto_feedback succeeds");

    assert_eq!(result["emitted"], json!(true), "got: {result}");
    let event_id = uuid::Uuid::parse_str(
        result["event_id"]
            .as_str()
            .expect("response must carry event_id"),
    )
    .expect("event_id must be a uuid");
    let event = rt
        .events(&token)
        .expect("event store")
        .get_event(event_id)
        .await
        .expect("get_event must not error")
        .expect("emitted event must be readable back");

    assert_eq!(
        event.payload["query"],
        json!("recall calibration target"),
        "emitted event must persist the serving query, not drop it: {:?}",
        event.payload
    );
    assert_eq!(
        event.payload["candidate_ids"],
        json!([first, second]),
        "emitted event must persist every candidate id, not just the selected result: {:?}",
        event.payload
    );
}

/// #1587: the canonical event verb remains `brain.feedback` for replay, while
/// the payload records whether direct or automatic feedback originated it.
#[tokio::test]
async fn feedback_events_record_and_count_the_originating_verb() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let direct_target = create_test_entity(&rt, &token).await;
    let automatic_target = create_test_entity(&rt, &token).await;
    let alias_target = create_test_entity(&rt, &token).await;

    let direct = pack
        .dispatch(
            "brain.feedback",
            json!({"target_id": direct_target, "signal": "useful"}),
            &registry,
            &token,
        )
        .await
        .expect("direct feedback succeeds");
    let automatic = pack
        .dispatch(
            "brain.auto_feedback",
            json!({
                "query": "origin attribution",
                "results": [{"id": automatic_target}],
                "target_id": automatic_target,
                "signal": "implicit_positive"
            }),
            &registry,
            &token,
        )
        .await
        .expect("automatic feedback succeeds");
    let alias = pack
        .dispatch(
            "brain.emit",
            json!({"target_id": alias_target, "signal": "useful"}),
            &registry,
            &token,
        )
        .await
        .expect("deprecated feedback alias succeeds");

    let store = rt.events(&token).expect("event store");
    for (response, expected_origin) in [
        (&direct, "brain.feedback"),
        (&automatic, "brain.auto_feedback"),
        (&alias, "brain.emit"),
    ] {
        let event_id = uuid::Uuid::parse_str(
            response["event_id"]
                .as_str()
                .expect("feedback response must carry event_id"),
        )
        .expect("event_id must be a UUID");
        let event = store
            .get_event(event_id)
            .await
            .expect("event read must succeed")
            .expect("feedback event must exist");
        assert_eq!(event.verb, "brain.feedback");
        assert_eq!(event.payload["originating_verb"], json!(expected_origin));
    }

    let counts = pack
        .dispatch(
            "brain.event_counts",
            json!({
                "since": "1970-01-01T00:00:00Z",
                "kind": "feedback_explicit"
            }),
            &registry,
            &token,
        )
        .await
        .expect("event counts succeed");
    assert_eq!(counts["counts_by_verb"]["brain.feedback"], json!(3));
    assert_eq!(
        counts["feedback_by_originating_verb"]["brain.feedback"],
        json!(1)
    );
    assert_eq!(
        counts["feedback_by_originating_verb"]["brain.auto_feedback"],
        json!(1)
    );
    assert_eq!(
        counts["feedback_by_originating_verb"]["brain.emit"],
        json!(1)
    );
}

#[tokio::test]
async fn feedback_originating_verb_is_not_a_forgeable_public_parameter() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;

    let error = pack
        .dispatch(
            "brain.feedback",
            json!({
                "target_id": target,
                "signal": "useful",
                "originating_verb": "brain.auto_feedback"
            }),
            &registry,
            &token,
        )
        .await
        .expect_err("callers must not be able to forge feedback origin");
    let message = error.to_string();
    assert!(message.contains("unknown field") && message.contains("originating_verb"));

    let events = rt
        .events(&token)
        .expect("event store")
        .query_events(
            khive_storage::event::EventFilter {
                kinds: vec![khive_types::EventKind::FeedbackExplicit],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .expect("feedback event query");
    assert!(events.items.is_empty());
}

/// #38: `memory.recall` fans results out across namespaces by design (root
/// CLAUDE.md "Rule 3b"), so a caller's `auto_feedback` on a just-recalled
/// short-prefix id must not fail just because the record lives outside the
/// caller's OWN namespace. Prefix resolution here must match the by-ID
/// contract's full-UUID path, which is already namespace-agnostic.
#[tokio::test]
async fn brain_auto_feedback_resolves_short_prefix_across_namespaces() {
    use core::convert::TryFrom;
    use khive_runtime::Namespace;

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let pack = BrainPack::new(rt.clone());
    let registry = empty_registry();

    let ns_a = Namespace::try_from("ns-a").expect("namespace a");
    let ns_b = Namespace::try_from("ns-b").expect("namespace b");
    let token_a = rt.authorize(ns_a).expect("token a");
    let token_b = rt.authorize(ns_b).expect("token b");

    // Entity lives in namespace A.
    let target = create_test_entity(&rt, &token_a).await;
    let prefix = &target[..8];

    // Feedback dispatched under namespace B's token — the recalled record was
    // never in B's own namespace, mirroring recall's cross-namespace fan-out.
    let result = pack
        .dispatch(
            "brain.auto_feedback",
            json!({
                "query": "cross-namespace prefix resolution",
                "results": [{ "id": prefix }],
                "target_id": prefix,
                "signal": "implicit_positive"
            }),
            &registry,
            &token_b,
        )
        .await
        .expect("auto_feedback must resolve a short prefix across namespaces, not fail with \"no record matches id prefix\"");

    assert_eq!(result["emitted"], json!(true), "got: {result}");
    assert_eq!(
        result["target_id"].as_str().unwrap_or(""),
        target,
        "resolved target_id must match the entity created in the OTHER namespace"
    );
}

#[tokio::test]
async fn brain_auto_feedback_empty_results_returns_no_emit() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    let result = pack
        .dispatch(
            "brain.auto_feedback",
            json!({
                "query": "empty recall results",
                "results": []
            }),
            &registry,
            &token,
        )
        .await
        .expect("auto_feedback with empty results succeeds");

    assert_eq!(result["emitted"], json!(false));
    assert_eq!(result["reason"], json!("no_results"));

    let missing_target = pack
        .dispatch(
            "brain.auto_feedback",
            json!({
                "query": "empty recall results with judgment",
                "results": [],
                "signal": "implicit_positive"
            }),
            &registry,
            &token,
        )
        .await
        .expect_err("a signal on an empty result set still requires a target");
    assert!(missing_target
        .to_string()
        .contains("target_id` is required"));

    let target = create_test_entity(&rt, &token).await;
    let out_of_set = pack
        .dispatch(
            "brain.auto_feedback",
            json!({
                "query": "empty recall results with named judgment",
                "results": [],
                "target_id": target,
                "signal": "implicit_positive"
            }),
            &registry,
            &token,
        )
        .await
        .expect_err("an empty result set cannot contain the named target");
    assert!(out_of_set
        .to_string()
        .contains("does not match any results[].id or results[].full_id"));
    assert_eq!(pack.snapshot().balanced_recall.total_events, 0);
}

#[tokio::test]
async fn brain_auto_feedback_accepts_short_note_id_prefix() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;
    // Use 8-char prefix as Agent mode would return from memory.recall.
    let prefix = &target[..8];

    for candidate in [
        json!({"id": prefix}),
        json!({"id": prefix, "full_id": null}),
    ] {
        let result = pack
            .dispatch(
                "brain.auto_feedback",
                json!({
                    "query": "prefix resolution test",
                    "results": [candidate],
                    "target_id": prefix,
                    "signal": "implicit_positive"
                }),
                &registry,
                &token,
            )
            .await
            .expect("absent or null full_id preserves 8-char prefix resolution");

        assert_eq!(result["emitted"], json!(true));
        assert_eq!(result["target_id"], target);
    }
}

/// #1505 direct-call defense: PackRuntime callers must provide a token for the
/// explicit namespace instead of treating the business parameter as authority.
#[tokio::test]
async fn brain_auto_feedback_direct_dispatch_honors_exact_namespace() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let local_token = rt.authorize(Namespace::local()).expect("local token");
    let arm_token = rt
        .authorize(Namespace::parse("bench-arm-a").expect("arm namespace"))
        .expect("arm token");
    let target = create_test_entity(&rt, &local_token).await;

    pack.dispatch(
        "brain.auto_feedback",
        json!({
            "query": "direct namespace regression",
            "results": [{"id": target}],
            "target_id": target,
            "signal": "implicit_positive",
            "namespace": "bench-arm-a",
        }),
        &registry,
        &arm_token,
    )
    .await
    .expect("direct auto_feedback accepts matching namespace token");

    let local = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &local_token,
        )
        .await
        .expect("local profile");
    let arm = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &arm_token,
        )
        .await
        .expect("arm profile");

    assert_eq!(local["total_events"], json!(0u64));
    assert_eq!(arm["total_events"], json!(1u64));
}

/// #1505: an explicit namespace is not a capability. A direct caller cannot
/// use a local token to emit or fold feedback into another namespace.
#[tokio::test]
async fn brain_auto_feedback_direct_dispatch_rejects_namespace_token_mismatch() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let local_token = rt.authorize(Namespace::local()).expect("local token");
    let arm_token = rt
        .authorize(Namespace::parse("bench-arm-a").expect("arm namespace"))
        .expect("arm token");
    let target = create_test_entity(&rt, &local_token).await;

    let local_before = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &local_token,
        )
        .await
        .expect("local profile before mismatch");
    let arm_before = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &arm_token,
        )
        .await
        .expect("arm profile before mismatch");
    let local_log_before = pack
        .dispatch(
            "brain.events",
            json!({"limit": 100}),
            &registry,
            &local_token,
        )
        .await
        .expect("local event log before mismatch");
    let arm_log_before = pack
        .dispatch("brain.events", json!({"limit": 100}), &registry, &arm_token)
        .await
        .expect("arm event log before mismatch");

    let err = pack
        .dispatch(
            "brain.auto_feedback",
            json!({
                "query": "unauthorized direct namespace",
                "results": [{"id": target}],
                "target_id": target,
                "signal": "implicit_positive",
                "namespace": "bench-arm-a",
            }),
            &registry,
            &local_token,
        )
        .await
        .expect_err("local token must not authorize arm feedback");
    assert!(
        matches!(err, RuntimeError::InvalidInput(ref msg) if msg.contains("does not match authorized token namespace")),
        "expected namespace mismatch InvalidInput, got {err:?}"
    );

    let local_after = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &local_token,
        )
        .await
        .expect("local profile after mismatch");
    let arm_after = pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": "balanced-recall-v1"}),
            &registry,
            &arm_token,
        )
        .await
        .expect("arm profile after mismatch");
    let local_log_after = pack
        .dispatch(
            "brain.events",
            json!({"limit": 100}),
            &registry,
            &local_token,
        )
        .await
        .expect("local event log after mismatch");
    let arm_log_after = pack
        .dispatch("brain.events", json!({"limit": 100}), &registry, &arm_token)
        .await
        .expect("arm event log after mismatch");

    assert_eq!(
        local_after["total_events"], local_before["total_events"],
        "rejected feedback must not mutate local posterior state"
    );
    assert_eq!(
        arm_after["total_events"], arm_before["total_events"],
        "rejected feedback must not mutate arm posterior state"
    );
    assert_eq!(
        local_log_after["count"], local_log_before["count"],
        "rejected feedback must not append a local event"
    );
    assert_eq!(
        arm_log_after["count"], arm_log_before["count"],
        "rejected feedback must not append an arm event"
    );
}

include!("tests/namespace_snapshot_tests.rs");

#[cfg(test)]
#[path = "help_tests.rs"]
mod help_tests;

include!("tests/seed_prior_admission_tests.rs");

// ── Concurrency / publication-atomicity regression tests ──────────────────────
//
// These tests were added to guard against the TOCTOU race in ensure_loaded where
// active_namespace, *state, and loaded_namespaces were updated in three separate
// critical sections.  After the fix all three are updated atomically while the
// tracker lock is held, so a concurrent dispatch can never observe active=true
// with stale state.

/// Invariant: after `ensure_loaded` returns, the tracker and shared state
/// must be in a consistent three-way view: `active_namespace == namespace`,
/// `loaded_namespaces` contains `namespace`, and the shared `BrainState` is
/// the one for that namespace. Exercised across an A→B→A load sequence. See
/// crates/khive-pack-brain/docs/dispatch-namespace-isolation.md#ensure_loaded_publication_is_atomic.
#[tokio::test]
async fn ensure_loaded_publication_is_atomic() {
    use core::convert::TryFrom;
    use khive_runtime::Namespace;

    let (_, rt) = make_pack_with_actor("actor-a");
    let pack = BrainPack::new(rt.clone());
    let registry = empty_registry();

    let ns_a = Namespace::try_from("atomic-ns-a").expect("namespace a");
    let ns_b = Namespace::try_from("atomic-ns-b").expect("namespace b");
    let token_a = rt.authorize(ns_a).expect("token a");
    let token_b = rt.authorize(ns_b).expect("token b");

    // Load namespace A by dispatching a read verb.
    pack.ensure_loaded(&token_a)
        .await
        .expect("ensure_loaded ns-a");

    // Invariant 1: after ensure_loaded, tracker must be fully consistent for A.
    {
        let t = pack.persistence.lock().unwrap();
        assert_eq!(
            t.active_namespace.as_deref(),
            Some("atomic-ns-a"),
            "active_namespace must equal requested namespace immediately after ensure_loaded"
        );
        assert!(
            t.loaded_namespaces.contains_key("atomic-ns-a"),
            "loaded_namespaces must contain the namespace immediately after ensure_loaded"
        );
    }
    // The shared state must also reflect A (profile list should be visible).
    {
        let s = pack.state.lock().unwrap();
        assert!(
            !s.profiles.is_empty(),
            "shared state must contain at least the built-in profile after loading ns-a"
        );
    }

    // Write something observable into A: create a binding.
    pack.dispatch(
        "brain.bind",
        json!({
            "profile_id": "balanced-recall-v1",
            "actor": "actor-a",
            "namespace": "atomic-ns-a",
            "consumer_kind": "recall",
        }),
        &registry,
        &token_a,
    )
    .await
    .expect("bind in namespace A");

    // Load namespace B.
    pack.ensure_loaded(&token_b)
        .await
        .expect("ensure_loaded ns-b");

    // Invariant 2: tracker must be fully consistent for B.
    {
        let t = pack.persistence.lock().unwrap();
        assert_eq!(
            t.active_namespace.as_deref(),
            Some("atomic-ns-b"),
            "active_namespace must equal namespace B after switching to B"
        );
        assert!(
            t.loaded_namespaces.contains_key("atomic-ns-b"),
            "loaded_namespaces must contain ns-b after ensure_loaded"
        );
        assert!(
            t.loaded_namespaces.contains_key("atomic-ns-a"),
            "loaded_namespaces must still contain ns-a (saved, not evicted)"
        );
    }
    // The shared state must reflect B: B has no bindings (was never written to).
    let bindings_b = pack
        .dispatch("brain.bindings", json!({}), &registry, &token_b)
        .await
        .expect("bindings in B");
    assert_eq!(
        bindings_b["count"],
        json!(0u64),
        "namespace B must see 0 bindings; the binding created in A must not bleed through"
    );

    // Switch back to namespace A — the save-restore path.
    pack.ensure_loaded(&token_a)
        .await
        .expect("ensure_loaded ns-a again");

    // Invariant 3: tracker consistent for A again.
    {
        let t = pack.persistence.lock().unwrap();
        assert_eq!(
            t.active_namespace.as_deref(),
            Some("atomic-ns-a"),
            "active_namespace must be restored to A"
        );
    }
    // The binding we created in A must still be present (save-restore preserved state).
    let bindings_a = pack
        .dispatch("brain.bindings", json!({}), &registry, &token_a)
        .await
        .expect("bindings in A after restore");
    assert_eq!(
        bindings_a["count"],
        json!(1u64),
        "binding created in namespace A must survive the save-restore round-trip"
    );
}

/// Concurrent dispatches to the same namespace must not observe active=true
/// with stale state.  We spawn N tasks that all call ensure_loaded for the
/// same namespace concurrently; after they all complete the state must be
/// consistent (tracker and shared state agree, no panic, no data loss).
#[tokio::test]
async fn ensure_loaded_concurrent_same_namespace_is_safe() {
    use core::convert::TryFrom;
    use khive_runtime::Namespace;
    use std::sync::Arc;

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let pack = Arc::new(BrainPack::new(rt.clone()));
    let registry = empty_registry();

    let ns = Namespace::try_from("conc-ns").expect("conc namespace");
    let token = rt.authorize(ns).expect("conc token");
    let token = Arc::new(token);

    // Spawn 8 concurrent ensure_loaded calls for the same namespace.
    let mut handles = Vec::new();
    for _ in 0..8 {
        let pack2 = Arc::clone(&pack);
        let tok2 = Arc::clone(&token);
        handles.push(tokio::spawn(
            async move { pack2.ensure_loaded(&tok2).await },
        ));
    }
    for h in handles {
        h.await
            .expect("task did not panic")
            .expect("ensure_loaded must not error");
    }

    // After all concurrent loads complete, tracker and state must agree.
    {
        let t = pack.persistence.lock().unwrap();
        assert_eq!(
            t.active_namespace.as_deref(),
            Some("conc-ns"),
            "active_namespace must be conc-ns after concurrent loads"
        );
        assert!(
            t.loaded_namespaces.contains_key("conc-ns"),
            "loaded_namespaces must contain conc-ns"
        );
    }
    {
        let s = pack.state.lock().unwrap();
        assert!(
            !s.profiles.is_empty(),
            "shared state must be non-empty (built-in profile present)"
        );
    }

    // A dispatch against the namespace must succeed without panicking.
    let result = pack
        .dispatch("brain.profiles", json!({}), &registry, &token)
        .await
        .expect("brain.profiles after concurrent ensure_loaded");
    assert!(
        result["count"].as_u64().unwrap_or(0) >= 1,
        "at least the built-in profile must be present"
    );
}

#[tokio::test]
async fn warm_pack_refreshes_profiles_and_bindings_written_by_peer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = khive_runtime::RuntimeConfig {
        db_path: Some(dir.path().join("cross-process-visibility.db")),
        actor_id: Some("peer-actor".to_string()),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    };
    let writer_rt = KhiveRuntime::new(config.clone()).expect("writer runtime");
    let reader_rt = KhiveRuntime::new(config).expect("reader runtime");
    let writer = BrainPack::new(writer_rt.clone());
    let reader = BrainPack::new(reader_rt.clone());
    let registry = empty_registry();
    let writer_token = writer_rt
        .authorize(Namespace::local())
        .expect("writer token");
    let reader_token = reader_rt
        .authorize(Namespace::local())
        .expect("reader token");

    let initial = reader
        .dispatch("brain.profiles", json!({}), &registry, &reader_token)
        .await
        .expect("warm reader profile registry");
    assert_eq!(initial["count"], json!(1u64));

    writer
        .dispatch(
            "brain.create_profile",
            json!({
                "name": "peer-visible-profile",
                "consumer_kind": "recall",
            }),
            &registry,
            &writer_token,
        )
        .await
        .expect("peer creates profile");

    let refreshed = reader
        .dispatch("brain.profiles", json!({}), &registry, &reader_token)
        .await
        .expect("warm reader refreshes profile registry");
    assert!(
        refreshed["profiles"]
            .as_array()
            .expect("profiles array")
            .iter()
            .any(|profile| profile["id"] == json!("peer-visible-profile")),
        "a warm pack must observe a profile committed by another pack"
    );

    let target = create_test_entity(&writer_rt, &writer_token).await;
    let feedback = reader
        .dispatch(
            "brain.feedback",
            json!({
                "target_id": target,
                "signal": "useful",
                "served_by_profile_id": "peer-visible-profile",
            }),
            &registry,
            &reader_token,
        )
        .await
        .expect("warm peer accepts feedback for newly visible profile");
    assert_eq!(feedback["emitted"], json!(true));

    let profile = writer
        .dispatch(
            "brain.profile",
            json!({"profile_id": "peer-visible-profile"}),
            &registry,
            &writer_token,
        )
        .await
        .expect("original writer refreshes peer feedback");
    assert_eq!(profile["total_events"], json!(1u64));

    writer
        .dispatch(
            "brain.bind",
            json!({
                "profile_id": "peer-visible-profile",
                "actor": "peer-actor",
                "namespace": "local",
                "consumer_kind": "recall",
            }),
            &registry,
            &writer_token,
        )
        .await
        .expect("peer creates binding");

    let bindings = reader
        .dispatch(
            "brain.bindings",
            json!({"profile_id": "peer-visible-profile"}),
            &registry,
            &reader_token,
        )
        .await
        .expect("warm reader refreshes bindings");
    assert_eq!(bindings["count"], json!(1u64));
    assert_eq!(bindings["bindings"][0]["actor"], json!("peer-actor"));
}

#[tokio::test]
async fn stale_pack_mutation_rebases_on_durable_snapshot() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = khive_runtime::RuntimeConfig {
        db_path: Some(dir.path().join("stale-writer.db")),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    };
    let first_rt = KhiveRuntime::new(config.clone()).expect("first runtime");
    let stale_rt = KhiveRuntime::new(config.clone()).expect("stale runtime");
    let observer_rt = KhiveRuntime::new(config).expect("observer runtime");
    let first = BrainPack::new(first_rt.clone());
    let stale = BrainPack::new(stale_rt.clone());
    let observer = BrainPack::new(observer_rt.clone());
    let registry = empty_registry();
    let first_token = first_rt.authorize(Namespace::local()).expect("first token");
    let stale_token = stale_rt.authorize(Namespace::local()).expect("stale token");
    let observer_token = observer_rt
        .authorize(Namespace::local())
        .expect("observer token");

    first
        .dispatch("brain.profiles", json!({}), &registry, &first_token)
        .await
        .expect("warm first pack");
    stale
        .dispatch("brain.profiles", json!({}), &registry, &stale_token)
        .await
        .expect("warm stale pack before either write");

    first
        .handle_create_profile(
            &first_token,
            json!({"name": "first-process-profile", "consumer_kind": "recall"}),
        )
        .await
        .expect("first process writes profile");

    // Call the handler directly so this pack deliberately skips dispatch's
    // freshness check. The persistence transaction itself must still read the
    // latest durable snapshot before applying its mutation.
    stale
        .handle_create_profile(
            &stale_token,
            json!({"name": "stale-process-profile", "consumer_kind": "recall"}),
        )
        .await
        .expect("stale process rebases its mutation");

    let profiles = observer
        .dispatch("brain.profiles", json!({}), &registry, &observer_token)
        .await
        .expect("fresh observer loads merged registry");
    let ids: std::collections::HashSet<&str> = profiles["profiles"]
        .as_array()
        .expect("profiles array")
        .iter()
        .filter_map(|profile| profile["id"].as_str())
        .collect();
    assert!(ids.contains("first-process-profile"));
    assert!(ids.contains("stale-process-profile"));
}

#[tokio::test]
#[serial_test::serial(brain_feedback_precommit)]
async fn feedback_revalidates_lifecycle_inside_cross_process_transaction() {
    use khive_storage::types::{SqlStatement, SqlValue};
    use std::sync::Arc;
    use tokio::sync::oneshot;

    struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            crate::pack::clear_feedback_precommit_hook();
        }
    }
    let _guard = HookGuard;

    let dir = tempfile::tempdir().expect("tempdir");
    let config = khive_runtime::RuntimeConfig {
        db_path: Some(dir.path().join("feedback-archive-race.db")),
        ..khive_runtime::RuntimeConfig::no_embeddings()
    };
    let feedback_rt = KhiveRuntime::new(config.clone()).expect("feedback runtime");
    let archive_rt = KhiveRuntime::new(config.clone()).expect("archive runtime");
    let observer_rt = KhiveRuntime::new(config).expect("observer runtime");
    let feedback_pack = Arc::new(BrainPack::new(feedback_rt.clone()));
    let archive_pack = BrainPack::new(archive_rt.clone());
    let observer_pack = BrainPack::new(observer_rt.clone());
    let registry = Arc::new(empty_registry());
    let feedback_token = Arc::new(
        feedback_rt
            .authorize(Namespace::local())
            .expect("feedback token"),
    );
    let archive_token = archive_rt
        .authorize(Namespace::local())
        .expect("archive token");
    let observer_token = observer_rt
        .authorize(Namespace::local())
        .expect("observer token");
    let profile_id = "feedback-archive-race-profile";

    feedback_pack
        .dispatch(
            "brain.create_profile",
            json!({"name": profile_id, "consumer_kind": "recall"}),
            &registry,
            &feedback_token,
        )
        .await
        .expect("create feedback profile");
    archive_pack
        .dispatch("brain.profiles", json!({}), &registry, &archive_token)
        .await
        .expect("archive runtime observes profile");
    let target = create_test_entity(&feedback_rt, &feedback_token).await;

    let (reached_tx, reached_rx) = oneshot::channel();
    let (proceed_tx, proceed_rx) = oneshot::channel();
    crate::pack::set_feedback_precommit_hook(crate::pack::FeedbackPrecommitHook {
        profile_id: profile_id.to_string(),
        reached_tx,
        proceed_rx,
    });

    let feedback_pack_task = Arc::clone(&feedback_pack);
    let feedback_token_task = Arc::clone(&feedback_token);
    let registry_task = Arc::clone(&registry);
    let target_for_task = target.clone();
    let feedback_usage = khive_storage::usage::UsageContext::new();
    let feedback_usage_task = feedback_usage.clone();
    let feedback_task = tokio::spawn(khive_storage::usage::scope(
        feedback_usage_task,
        async move {
            feedback_pack_task
                .dispatch(
                    "brain.feedback",
                    json!({
                        "target_id": target_for_task,
                        "signal": "implicit_positive",
                        "served_by_profile_id": profile_id,
                    }),
                    &registry_task,
                    &feedback_token_task,
                )
                .await
        },
    ));

    reached_rx
        .await
        .expect("feedback reaches post-preflight hook");
    archive_pack
        .dispatch(
            "brain.archive",
            json!({"profile_id": profile_id}),
            &registry,
            &archive_token,
        )
        .await
        .expect("peer archives inactive profile");
    proceed_tx.send(()).expect("release feedback transaction");

    let error = feedback_task
        .await
        .expect("feedback task joins")
        .expect_err("rebased feedback must reject the archived profile");
    assert!(
        matches!(error, RuntimeError::InvalidInput(ref message) if message.contains("archived")),
        "rebased feedback rejection must report the archived lifecycle: {error:?}"
    );
    assert!(
        feedback_usage.snapshot().get("event_rows").is_none(),
        "a lifecycle rejection inside the atomic unit must not count an event row"
    );

    let profile = observer_pack
        .dispatch(
            "brain.profile",
            json!({"profile_id": profile_id}),
            &registry,
            &observer_token,
        )
        .await
        .expect("observer reads archived profile");
    assert_eq!(profile["lifecycle"], json!("archived"));
    assert_eq!(profile["total_events"], json!(0u64));

    let mut reader = observer_rt.sql().reader().await.expect("observer reader");
    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT \
                    (SELECT COUNT(*) FROM events \
                     WHERE namespace = ?1 AND verb = 'brain.feedback') AS public_events, \
                    (SELECT COUNT(*) FROM brain_event_log \
                     WHERE namespace = ?1 AND event_kind = 'brain.feedback') AS private_events, \
                    (SELECT COUNT(*) FROM brain_implicit_mass \
                     WHERE namespace = ?1 AND profile_id = ?2 AND target_id = ?3) AS mass_rows"
                .into(),
            params: vec![
                SqlValue::Text(observer_token.namespace().as_str().to_string()),
                SqlValue::Text(profile_id.to_string()),
                SqlValue::Text(target),
            ],
            label: Some("brain_test_feedback_archive_race_counts".into()),
        })
        .await
        .expect("read feedback side-effect counts")
        .expect("count row");
    assert!(matches!(
        row.get("public_events"),
        Some(SqlValue::Integer(0))
    ));
    assert!(matches!(
        row.get("private_events"),
        Some(SqlValue::Integer(0))
    ));
    assert!(matches!(row.get("mass_rows"), Some(SqlValue::Integer(0))));
}

#[tokio::test]
async fn matching_generation_reuses_local_snapshot_without_decoding_durable_json() {
    use khive_storage::types::{SqlStatement, SqlValue};

    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).expect("local token");

    pack.dispatch(
        "brain.create_profile",
        json!({"name": "generation-reuse-seed", "consumer_kind": "recall"}),
        &registry,
        &token,
    )
    .await
    .expect("seed durable snapshot and local generation");

    // Poison only the durable JSON while preserving updated_at. This is a
    // decode sentinel, not a supported external mutation: the next dispatch's
    // generation check still matches the live local snapshot, so the mutation
    // transaction must not fetch or deserialize this blob.
    {
        let mut writer = rt.sql().writer().await.expect("writer");
        let changed = writer
            .execute(SqlStatement {
                sql: "UPDATE brain_profile_snapshots SET snapshot_json = ?1 \
                      WHERE profile_id = ?2 AND namespace = ?3"
                    .into(),
                params: vec![
                    SqlValue::Text("{decode-sentinel".to_string()),
                    SqlValue::Text("__brain__".to_string()),
                    SqlValue::Text(token.namespace().as_str().to_string()),
                ],
                label: Some("brain_test_poison_matching_snapshot".into()),
            })
            .await
            .expect("install decode sentinel without advancing generation");
        assert_eq!(changed, 1, "decode sentinel must replace the snapshot row");
    }

    pack.dispatch(
        "brain.create_profile",
        json!({"name": "generation-reuse-next", "consumer_kind": "recall"}),
        &registry,
        &token,
    )
    .await
    .expect("matching generation must reuse local snapshot");

    let (snapshot, _) = crate::persist::load_latest_snapshot(
        rt.sql().as_ref(),
        token.namespace().as_str(),
        ENTITY_CACHE_CAPACITY,
    )
    .await
    .expect("load replacement snapshot")
    .expect("replacement snapshot exists");
    assert!(snapshot.profiles.contains_key("generation-reuse-seed"));
    assert!(snapshot.profiles.contains_key("generation-reuse-next"));
}

#[tokio::test]
async fn mutation_timestamp_advances_past_existing_snapshot() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = rt.authorize(Namespace::local()).expect("local token");
    let namespace = token.namespace().as_str();
    let future_updated_at = chrono::Utc::now().timestamp_micros() + 1_000_000;
    let snapshot = khive_brain_core::BrainState::new(ENTITY_CACHE_CAPACITY).to_snapshot();
    crate::persist::upsert_snapshot(rt.sql().as_ref(), namespace, &snapshot, future_updated_at)
        .await
        .expect("seed future-dated snapshot");

    let tracker = std::sync::Mutex::new(crate::persist::PersistenceTracker::new());
    let state = std::sync::Mutex::new(khive_brain_core::BrainState::new(ENTITY_CACHE_CAPACITY));
    let event = khive_storage::event::Event::new(
        namespace,
        "brain.test_monotonic_timestamp",
        khive_types::EventKind::Audit,
        khive_types::SubstrateKind::Event,
        "brain",
    )
    .with_payload(json!({"probe": true}));
    crate::persist::persist_brain_state_mutation(
        rt.sql().as_ref(),
        &token,
        &tracker,
        &state,
        crate::persist::BrainMutationEvent {
            profile_id: "balanced-recall-v1".to_string(),
            event_kind: event.verb.clone(),
            payload: serde_json::to_value(event).expect("test event serializes"),
        },
        ENTITY_CACHE_CAPACITY,
        |_state| Ok::<(), RuntimeError>(()),
    )
    .await
    .expect("persist mutation after future-dated snapshot");

    let (_, updated_at) =
        crate::persist::load_latest_snapshot(rt.sql().as_ref(), namespace, ENTITY_CACHE_CAPACITY)
            .await
            .expect("load snapshot")
            .expect("snapshot exists");
    assert!(updated_at > future_updated_at);

    let replay = crate::persist::load_events_since(rt.sql().as_ref(), namespace, future_updated_at)
        .await
        .expect("load post-snapshot events");
    assert_eq!(replay.events.len(), 1);
}

/// Cross-namespace concurrent loads must not cause one namespace to save the
/// wrong state under saved_states.  We alternate between two namespaces while
/// mutating each, then verify each namespace sees only its own state.
#[tokio::test]
async fn ensure_loaded_cross_namespace_concurrent_does_not_corrupt_saved_states() {
    use core::convert::TryFrom;
    use khive_runtime::Namespace;

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let pack = std::sync::Arc::new(BrainPack::new(rt.clone()));
    let registry = empty_registry();

    let ns_x = Namespace::try_from("xns-x").expect("x");
    let ns_y = Namespace::try_from("xns-y").expect("y");
    let (_, actor_x_rt) = make_pack_with_actor("x-actor");
    let (_, actor_y_rt) = make_pack_with_actor("y-actor");
    let token_x = actor_x_rt.authorize(ns_x).expect("token x");
    let token_y = actor_y_rt.authorize(ns_y).expect("token y");
    let token_x = std::sync::Arc::new(token_x);
    let token_y = std::sync::Arc::new(token_y);

    // Interleave loads: X then Y then X then Y.
    pack.ensure_loaded(&token_x).await.expect("load x");
    pack.dispatch(
        "brain.bind",
        json!({"profile_id": "balanced-recall-v1", "actor": "x-actor", "namespace": "xns-x", "consumer_kind": "recall"}),
        &registry,
        &token_x,
    )
    .await
    .expect("bind x");

    pack.ensure_loaded(&token_y).await.expect("load y");
    // Y has no bindings yet; add one so it has observable state too.
    pack.dispatch(
        "brain.bind",
        json!({"profile_id": "balanced-recall-v1", "actor": "y-actor", "namespace": "xns-y", "consumer_kind": "recall"}),
        &registry,
        &token_y,
    )
    .await
    .expect("bind y");

    // Switch back to X and verify its binding survived.
    pack.ensure_loaded(&token_x).await.expect("reload x");
    let bx = pack
        .dispatch("brain.bindings", json!({}), &registry, &token_x)
        .await
        .expect("bindings x");
    assert_eq!(
        bx["count"],
        json!(1u64),
        "namespace X must still have exactly 1 binding after cross-namespace interleave"
    );

    // Switch to Y and verify its binding survived.
    pack.ensure_loaded(&token_y).await.expect("reload y");
    let by = pack
        .dispatch("brain.bindings", json!({}), &registry, &token_y)
        .await
        .expect("bindings y");
    assert_eq!(
        by["count"],
        json!(1u64),
        "namespace Y must still have exactly 1 binding after cross-namespace interleave"
    );
}

/// Deterministic interleaving test for the concurrent cold-load race, using
/// the test-only `POST_LOAD_HOOK` in persist.rs to pause a loader mid-load.
/// See crates/khive-pack-brain/docs/dispatch-namespace-isolation.md#concurrent_cold_load_does_not_clobber_live_state.
#[tokio::test]
async fn concurrent_cold_load_does_not_clobber_live_state() {
    use core::convert::TryFrom;
    use khive_runtime::Namespace;
    use std::sync::Arc;
    use tokio::sync::oneshot;

    // Always clean up the hook, even on panic.
    struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            persist::clear_post_load_hook();
        }
    }
    let _guard = HookGuard;

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let pack = Arc::new(BrainPack::new(rt.clone()));
    let registry = empty_registry();

    let ns = Namespace::try_from("race-ns-det").expect("race namespace");
    let (_, actor_rt) = make_pack_with_actor("racer");
    let token = Arc::new(actor_rt.authorize(ns).expect("race token"));

    // ── Step 1: register the hook for Loader B ────────────────────────────────
    // B will fire the hook once it has finished the cold DB load.
    let (reached_tx, reached_rx) = oneshot::channel::<()>();
    let (proceed_tx, proceed_rx) = oneshot::channel::<()>();
    persist::set_post_load_hook(persist::LoadHook {
        reached_tx,
        proceed_rx,
    });

    // ── Step 2: spawn Loader B ────────────────────────────────────────────────
    // B will pause at the hook (awaiting proceed_rx) once the hook fires.
    let pack_b = Arc::clone(&pack);
    let token_b = Arc::clone(&token);
    let b_handle = tokio::spawn(async move { pack_b.ensure_loaded(&token_b).await });

    // Wait for B to signal it has reached the hook (DB load done, not yet in
    // the final tracker block).
    reached_rx.await.expect("loader B must signal reached");

    // ── Step 3: run Loader A to completion ────────────────────────────────────
    // The hook was consumed by B's `.take()`, so A passes straight through.
    // A publishes "race-ns-det" as active and returns.
    pack.ensure_loaded(&token)
        .await
        .expect("loader A: ensure_loaded");

    // ── Step 4: mutate state under A's namespace ──────────────────────────────
    pack.dispatch(
        "brain.bind",
        json!({
            "profile_id": "balanced-recall-v1",
            "actor": "racer",
            "namespace": "race-ns-det",
            "consumer_kind": "recall",
        }),
        &registry,
        &token,
    )
    .await
    .expect("brain.bind after A loaded");

    // Binding count is now 1.
    let before = pack
        .dispatch("brain.bindings", json!({}), &registry, &token)
        .await
        .expect("bindings before B resumes");
    assert_eq!(
        before["count"],
        json!(1u64),
        "binding must exist before B resumes"
    );

    // ── Step 5: release Loader B ──────────────────────────────────────────────
    // B now enters the final tracker block.
    // OLD code: B overwrites *state with its stale cold-loaded copy → count=0 → FAIL.
    // FIXED code: B re-checks is_active("race-ns-det") → true → returns early → count=1 → PASS.
    proceed_tx.send(()).expect("send proceed to B");
    b_handle
        .await
        .expect("loader B task must not panic")
        .expect("loader B ensure_loaded must not error");

    // ── Step 6: assert the mutation survived ─────────────────────────────────
    let after = pack
        .dispatch("brain.bindings", json!({}), &registry, &token)
        .await
        .expect("bindings after B resumes");
    assert_eq!(
        after["count"],
        json!(1u64),
        "concurrent cold-load race: Loader B must not clobber the binding \
         created by Loader A (old code would return 0 here)"
    );
}

/// Proves the dispatch atomicity race EXISTS without the gate: manually
/// sequences ensure_loaded + handler calls (bypassing `dispatch()`'s gate)
/// to deterministically reproduce a cross-namespace bookkeeping corruption.
/// See crates/khive-pack-brain/docs/dispatch-namespace-isolation.md#dispatch_gate_race_is_observable_without_gate.
#[tokio::test]
async fn dispatch_gate_race_is_observable_without_gate() {
    use core::convert::TryFrom;
    use khive_runtime::Namespace;

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let pack = BrainPack::new(rt.clone());
    let registry = empty_registry();

    let ns_a = Namespace::try_from("bare-ns-a").expect("ns-a");
    let ns_b = Namespace::try_from("bare-ns-b").expect("ns-b");
    let (_, actor_rt) = make_pack_with_actor("racer-a");
    let token_a = actor_rt.authorize(ns_a).expect("token a");
    let token_b = actor_rt.authorize(ns_b).expect("token b");

    // Step 1: A calls ensure_loaded(ns-a). Slot is now ns-a.
    pack.ensure_loaded(&token_a).await.expect("ensure_loaded a");

    // Step 2: B calls ensure_loaded(ns-b). Slot is now ns-b.
    // (This is the interleaving that happens between ensure_loaded and
    // the handler in a concurrent dispatch — the gate prevents this.)
    pack.ensure_loaded(&token_b).await.expect("ensure_loaded b");

    // Step 3: A's handler now runs (binding for ns-a). But the slot is ns-b.
    // WITHOUT the gate, dispatch() lets this happen after a concurrent swap.
    // We reproduce it by calling handle_bind directly.
    pack.handle_bind(
        &token_a,
        json!({
            "profile_id": "balanced-recall-v1",
            "actor": "racer-a",
            "namespace": "bare-ns-a",
            "consumer_kind": "recall",
        }),
        &registry,
    )
    .await
    .expect("handle_bind for a");

    // Step 4: Assert the binding landed in the WRONG namespace slot (see
    // guide for the #457/#458 durability-fix-era corruption shape).
    let bindings_b_now = pack
        .dispatch("brain.bindings", json!({}), &registry, &token_b)
        .await
        .expect("bindings ns-b");
    assert_eq!(
        bindings_b_now["count"],
        json!(0u64),
        "race reproduced: ns-b's slot lost its content to the mislabeled save-restore"
    );

    let bindings_a_now = pack
        .dispatch("brain.bindings", json!({}), &registry, &token_a)
        .await
        .expect("bindings ns-a");
    assert_eq!(
        bindings_a_now["count"],
        json!(1u64),
        "race reproduced: A's leaked bind resurfaces under ns-a's slot instead"
    );
}

/// Proves the dispatch gate FIXES the race from
/// `dispatch_gate_race_is_observable_without_gate`: with the gate held
/// across ensure_loaded + handler, no concurrent namespace swap can occur.
/// See crates/khive-pack-brain/docs/dispatch-namespace-isolation.md#dispatch_gate_prevents_cross_namespace_slot_swap.
#[tokio::test]
async fn dispatch_gate_prevents_cross_namespace_slot_swap() {
    use core::convert::TryFrom;
    use khive_runtime::Namespace;
    use std::sync::Arc;
    use tokio::sync::oneshot;

    // Always clean up the hook, even on panic.
    struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            crate::pack::clear_dispatch_interleave_hook();
        }
    }
    let _guard = HookGuard;

    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let pack = Arc::new(BrainPack::new(rt.clone()));
    let registry = Arc::new(empty_registry());

    let ns_a = Namespace::try_from("gate-ns-a").expect("ns-a");
    let ns_b = Namespace::try_from("gate-ns-b").expect("ns-b");
    let (_, actor_a_rt) = make_pack_with_actor("racer-a");
    let (_, actor_b_rt) = make_pack_with_actor("racer-b");
    let token_a = Arc::new(actor_a_rt.authorize(ns_a).expect("token a"));
    let token_b = Arc::new(actor_b_rt.authorize(ns_b).expect("token b"));

    // ── Step 1: register the dispatch-gap hook for Dispatch A ─────────────────
    let (reached_tx, reached_rx) = oneshot::channel::<()>();
    let (proceed_tx, proceed_rx) = oneshot::channel::<()>();
    crate::pack::set_dispatch_interleave_hook(crate::pack::DispatchHook {
        reached_tx,
        proceed_rx,
    });

    // ── Step 2: spawn Dispatch A (brain.bind for ns-a) ────────────────────────
    // A will: acquire gate, ensure_loaded(ns-a), fire the hook, pause.
    let pack_a = Arc::clone(&pack);
    let token_a2 = Arc::clone(&token_a);
    let registry_a = Arc::clone(&registry);
    let a_handle = tokio::spawn(async move {
        pack_a
            .dispatch(
                "brain.bind",
                json!({
                    "profile_id": "balanced-recall-v1",
                    "actor": "racer-a",
                    "namespace": "gate-ns-a",
                    "consumer_kind": "recall",
                }),
                &registry_a,
                &token_a2,
            )
            .await
    });

    // Wait for A to signal it is at the dispatch gap (gate held, ns-a loaded).
    reached_rx.await.expect("dispatch A must reach hook");

    // ── Step 3: spawn Dispatch B (brain.bind for ns-b) ────────────────────────
    // B will attempt to acquire the dispatch gate → blocked until A releases.
    let pack_b = Arc::clone(&pack);
    let token_b2 = Arc::clone(&token_b);
    let registry_b = Arc::clone(&registry);
    let b_handle = tokio::spawn(async move {
        pack_b
            .dispatch(
                "brain.bind",
                json!({
                    "profile_id": "balanced-recall-v1",
                    "actor": "racer-b",
                    "namespace": "gate-ns-b",
                    "consumer_kind": "recall",
                }),
                &registry_b,
                &token_b2,
            )
            .await
    });

    // ── Step 4: release A, then await both ───────────────────────────────────
    // A's handler writes racer-a into ns-a, then releases the gate.
    // B is then unblocked; it ensure_loaded(ns-b) and writes racer-b.
    proceed_tx.send(()).expect("send proceed to A");
    a_handle
        .await
        .expect("dispatch A task must not panic")
        .expect("dispatch A must not error");
    b_handle
        .await
        .expect("dispatch B task must not panic")
        .expect("dispatch B must not error");

    // ── Step 5: assert each namespace sees exactly its own binding ─────────────
    let bindings_a = pack
        .dispatch("brain.bindings", json!({}), &registry, &token_a)
        .await
        .expect("bindings for ns-a");
    assert_eq!(
        bindings_a["count"],
        json!(1u64),
        "gate: ns-a must have exactly 1 binding (racer-a)"
    );

    let bindings_b = pack
        .dispatch("brain.bindings", json!({}), &registry, &token_b)
        .await
        .expect("bindings for ns-b");
    assert_eq!(
        bindings_b["count"],
        json!(1u64),
        "gate: ns-b must have exactly 1 binding (racer-b)"
    );
}

// ── BRAIN-005: section_signals validation ────────────────────────────────────

#[cfg(test)]
#[path = "brain_005_section_signals_tests.rs"]
mod brain_005_section_signals;

#[path = "semantic_section_feedback_tests.rs"]
mod semantic_section_feedback;

// Regression: brain.feedback must accept a short hex prefix for target_id,
// consistent with every other by-id verb (get, update, delete, link, …).
// Before the fix, `target_id.parse::<Uuid>()` rejected anything shorter than
// a full 36-char UUID with "invalid target_id: invalid length: ...".
#[tokio::test]
async fn feedback_accepts_short_prefix_target_id() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    let full_uuid = create_test_entity(&rt, &token).await;
    // Take the first 8 hex chars as the short prefix the agent would supply.
    let prefix = &full_uuid[..8];

    let result = pack
        .dispatch(
            "brain.feedback",
            json!({
                "target_id": prefix,
                "signal": "useful",
                "served_by_profile_id": "balanced-recall-v1"
            }),
            &registry,
            &token,
        )
        .await
        .expect("brain.feedback must accept an 8-char hex prefix for target_id");

    assert_eq!(result["emitted"], json!(true), "emitted must be true");
    assert_eq!(result["signal"], json!("useful"), "signal must round-trip");
}

struct AdapterWarningEmbeddingService;

#[async_trait::async_trait]
impl lattice_embed::EmbeddingService for AdapterWarningEmbeddingService {
    async fn embed(
        &self,
        texts: &[String],
        _model: lattice_embed::EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
        Ok(vec![vec![1.0]; texts.len()])
    }

    fn supports_model(&self, _model: lattice_embed::EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "adapter-warning-test"
    }
}

struct AdapterWarningEmbedderProvider;

#[async_trait::async_trait]
impl khive_runtime::EmbedderProvider for AdapterWarningEmbedderProvider {
    fn name(&self) -> &str {
        "adapter-warning-test"
    }

    fn dimensions(&self) -> usize {
        1
    }

    async fn build(
        &self,
    ) -> Result<std::sync::Arc<dyn lattice_embed::EmbeddingService>, RuntimeError> {
        Ok(std::sync::Arc::new(AdapterWarningEmbeddingService))
    }
}

#[tokio::test]
async fn register_adapter_reports_embedding_truncation() {
    let (pack, runtime) = make_pack();
    runtime.register_embedder(AdapterWarningEmbedderProvider);
    let registry = empty_registry();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let active_revision = std::env::var("KHIVE_BRAIN_BASE_MODEL_REVISION")
        .unwrap_or_else(|_| crate::handlers::DEFAULT_BASE_MODEL_REVISION.to_string());
    let adapter_id = "x".repeat(lattice_embed::MAX_TEXT_BYTES + 1);

    let response = pack
        .dispatch(
            "brain.register_adapter",
            json!({
                "adapter_id": adapter_id,
                "content_hash": "sha256:abcd1234",
                "base_model_revision": active_revision,
            }),
            &registry,
            &token,
        )
        .await
        .expect("register_adapter with an over-limit adapter ID");

    assert_eq!(response["registered"], json!(true));
    assert_eq!(response["adapter_id"].as_str(), Some(adapter_id.as_str()));
    assert_eq!(
        response["warnings"],
        json!([khive_runtime::retrieval::EMBEDDING_INPUT_TRUNCATED_WARNING]),
        "register_adapter must disclose a truncated embedding input: {response}"
    );
}

// ── #354: brain.register_adapter ─────────────────────────────────────────────

// ACCEPT: matching base_model_revision registers the adapter and persists the artifact.
#[tokio::test]
async fn register_adapter_accept_matching_revision() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    let active_rev = std::env::var("KHIVE_BRAIN_BASE_MODEL_REVISION")
        .unwrap_or_else(|_| crate::handlers::DEFAULT_BASE_MODEL_REVISION.to_string());

    let result = pack
        .dispatch(
            "brain.register_adapter",
            json!({
                "adapter_id": "lora-v1",
                "content_hash": "sha256:abcd1234",
                "base_model_revision": active_rev,
            }),
            &registry,
            &token,
        )
        .await
        .expect("register_adapter with matching revision must succeed");

    assert_eq!(result["registered"], json!(true), "registered must be true");
    assert_eq!(result["adapter_id"], json!("lora-v1"));
    assert_eq!(result["content_hash"], json!("sha256:abcd1234"));
    assert_eq!(result["base_model_revision"], json!(active_rev));

    // Verify the artifact entity was actually persisted.
    let entities = rt
        .list_entities(&token, Some("artifact"), Some("adapter"), 10, 0)
        .await
        .expect("list_entities must succeed");
    let found = entities.iter().any(|e| e.name == "lora-v1");
    assert!(
        found,
        "artifact entity 'lora-v1' must exist after successful registration"
    );
    let entity = entities.iter().find(|e| e.name == "lora-v1").unwrap();
    let props = entity
        .properties
        .as_ref()
        .expect("entity must have properties");
    assert_eq!(
        props["content_hash"],
        json!("sha256:abcd1234"),
        "content_hash must be stored in entity properties"
    );
}

// REJECT: mismatching base_model_revision returns an error and does not persist.
#[tokio::test]
async fn register_adapter_reject_mismatching_revision() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    let err = pack
        .dispatch(
            "brain.register_adapter",
            json!({
                "adapter_id": "lora-bad",
                "content_hash": "sha256:deadbeef",
                "base_model_revision": "wrong-revision-xyz",
            }),
            &registry,
            &token,
        )
        .await
        .unwrap_err();

    if let RuntimeError::InvalidInput(msg) = &err {
        assert!(
            msg.contains("mismatch") || msg.contains("expected"),
            "#354: rejection message must name the mismatch; got: {msg}"
        );
        assert!(
            msg.contains("wrong-revision-xyz") || msg.contains("base_model_revision"),
            "#354: rejection message must name the supplied revision; got: {msg}"
        );
    } else {
        panic!("#354: mismatching revision must return InvalidInput, got {err:?}");
    }

    // No artifact entity must have been persisted.
    let entities = rt
        .list_entities(&token, Some("artifact"), Some("adapter"), 10, 0)
        .await
        .expect("list_entities must succeed");
    assert!(
        !entities.iter().any(|e| e.name == "lora-bad"),
        "#354: rejected registration must not persist an artifact entity"
    );
}

// ── Router section-state regression tests (require lattice-router feature) ───
//
// These tests lock down the live handler path: section_signals sent to a
// serving profile must seed a section posterior entry on first contact and
// push the relevant posterior mean away from the default prior.  Without the
// entry-seeding fix, `balanced-recall-v1` silently dropped section_signals
// because it had no `section_states` entry, leaving the router context vector
// frozen at static priors.

#[cfg(feature = "lattice-router")]
#[path = "router_section_tests.rs"]
mod router_section_tests;

// ── ADR-081: recall retune driver — fold gate + scorer provenance ────────────

#[path = "adr081_retune_driver_tests.rs"]
mod adr081_retune_driver_tests;

// ── issues #457 / #458: durable-write helper regressions ─────────────────────

#[path = "durable_write_tests.rs"]
mod durable_write_tests;

// ── brain.event_counts (ADR-103 Stage 1, #724 Ask A) ────────────────────────

#[cfg(test)]
mod event_counts_tests {
    use super::*;
    use khive_runtime::micros_to_iso;
    use khive_storage::event::{Event, EventFilter};
    use khive_storage::types::PageRequest;
    use khive_types::{EventKind, SubstrateKind};
    use serde_json::Value;

    /// Append a synthetic event through the raw backend fixture seam, bypassing
    /// verb dispatch, so analytics tests can control actor, `created_at`, and
    /// payload precisely instead of relying on wall-clock timing. Production
    /// pack code must use `KhiveRuntime::events`, whose token-scoped decorator
    /// intentionally seals actor and namespace attribution.
    pub(super) async fn seed_event(
        rt: &KhiveRuntime,
        token: &NamespaceToken,
        verb: &str,
        kind: EventKind,
        actor: &str,
        created_at: i64,
        payload: Value,
    ) {
        let mut event = Event::new(
            token.namespace().as_str(),
            verb,
            kind,
            SubstrateKind::Note,
            actor,
        );
        event.created_at = created_at;
        event.payload = payload;
        if kind == EventKind::SearchExecuted && event.payload.get("result_kind").is_none() {
            event.payload["result_kind"] = json!("note");
        }
        rt.backend()
            .events_for_namespace(token.namespace().as_str())
            .expect("trusted fixture event store")
            .append_event(event)
            .await
            .expect("seed event append");
    }

    /// Sanity check that the seeding helper and window semantics agree with
    /// the underlying `EventFilter` used by other event-store tests: appending
    /// a handful of rows and reading them back via `query_events` with an
    /// explicit filter must see exactly what was seeded.
    async fn seeded_count(rt: &KhiveRuntime, token: &NamespaceToken) -> usize {
        rt.events(token)
            .expect("event store")
            .query_events(
                EventFilter::default(),
                PageRequest {
                    offset: 0,
                    limit: 1000,
                },
            )
            .await
            .expect("query_events")
            .items
            .len()
    }

    #[tokio::test]
    async fn window_boundaries_include_since_exclude_until() {
        let (pack, rt) = make_pack_with_actor("lambda:a");
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local()).unwrap();

        let since = 1_000_000_i64;
        let until = 2_000_000_i64;

        // Exactly at `since` — must be INCLUDED.
        seed_event(
            &rt,
            &token,
            "search",
            EventKind::SearchExecuted,
            "lambda:a",
            since,
            json!({}),
        )
        .await;
        // Inside the window.
        seed_event(
            &rt,
            &token,
            "search",
            EventKind::SearchExecuted,
            "lambda:a",
            1_500_000,
            json!({}),
        )
        .await;
        // Exactly at `until` — must be EXCLUDED (half-open window).
        seed_event(
            &rt,
            &token,
            "search",
            EventKind::SearchExecuted,
            "lambda:a",
            until,
            json!({}),
        )
        .await;
        // Before `since` — must be EXCLUDED.
        seed_event(
            &rt,
            &token,
            "search",
            EventKind::SearchExecuted,
            "lambda:a",
            500_000,
            json!({}),
        )
        .await;

        assert_eq!(seeded_count(&rt, &token).await, 4);

        let result = pack
            .dispatch(
                "brain.event_counts",
                json!({
                    "since": micros_to_iso(since),
                    "until": micros_to_iso(until),
                }),
                &registry,
                &token,
            )
            .await
            .expect("brain.event_counts must succeed");

        assert_eq!(
            result["total"],
            json!(2),
            "window must include the `since` boundary and exclude the `until` boundary: {result}"
        );
        assert_eq!(result["counts_by_kind"]["search_executed"], json!(2));
    }

    /// A count covering one namespace must say which one, and say what else
    /// the caller could ask for.
    ///
    /// Memory and brain verbs stamp the acting identity's namespace whenever a
    /// call carries one, so a caller reading the default scope can be missing a
    /// whole population of its own activity. The event window is opened on a
    /// single namespace, so a visibility set does NOT widen it the way it
    /// widens a note read: arm three below is the falsifier for that belief,
    /// and it is the reason the disclosure has to name the reachable
    /// namespaces rather than let the caller infer them from what came back.
    #[tokio::test]
    async fn counts_disclose_the_namespace_they_covered_and_the_way_to_ask() {
        let (pack, rt) = make_pack_with_actor("lambda:a");
        let registry = empty_registry();
        let here = rt.authorize(Namespace::local()).unwrap();
        let elsewhere = rt
            .authorize(Namespace::parse("other").expect("namespace"))
            .expect("authorize other");

        for at in [1_000_000_i64, 1_100_000] {
            seed_event(
                &rt,
                &here,
                "search",
                EventKind::SearchExecuted,
                "lambda:a",
                at,
                json!({}),
            )
            .await;
        }
        seed_event(
            &rt,
            &elsewhere,
            "search",
            EventKind::SearchExecuted,
            "lambda:a",
            1_200_000,
            json!({}),
        )
        .await;

        let window = json!({
            "since": micros_to_iso(900_000),
            "until": micros_to_iso(2_000_000),
        });

        // Arm 1: the default read. It covers `local` and names it.
        let narrow = pack
            .dispatch("brain.event_counts", window.clone(), &registry, &here)
            .await
            .expect("narrow read must succeed");
        assert_eq!(
            narrow["scope"]["namespace"],
            json!("local"),
            "the scope must state the namespace the answer was computed under: {narrow}"
        );
        assert_eq!(
            narrow["total"],
            json!(2),
            "the narrow read counts only what it could see: {narrow}"
        );

        // Arm 2: the way to ask. A token scoped to the other namespace returns
        // that namespace's row and nothing from `local`, so the population the
        // narrow read omitted is reachable rather than lost.
        let asked = pack
            .dispatch("brain.event_counts", window.clone(), &registry, &elsewhere)
            .await
            .expect("explicit-namespace read must succeed");
        assert_eq!(
            asked["scope"]["namespace"],
            json!("other"),
            "asking for a namespace must move the scope to it: {asked}"
        );
        assert_eq!(
            asked["total"],
            json!(1),
            "the asked-for namespace returns its own row, not the caller's: {asked}"
        );

        // Arm 3: visibility does not widen this window. The token below may
        // read `other`, and the count is still the `local` two — which is
        // exactly why `other_namespaces` has to be stated instead of inferred.
        let wide_token = rt
            .authorize_with_visibility(
                Namespace::local(),
                vec![Namespace::parse("other").expect("namespace")],
            )
            .expect("authorize with visibility");
        let wide = pack
            .dispatch("brain.event_counts", window, &registry, &wide_token)
            .await
            .expect("wide read must succeed");
        assert_eq!(
            wide["total"],
            json!(2),
            "a visibility set must not widen a single-namespace event window: {wide}"
        );
        assert_eq!(
            wide["scope"]["other_namespaces"],
            json!(["other"]),
            "the scope must name the namespaces this caller can ask for: {wide}"
        );
    }

    #[tokio::test]
    async fn empty_window_returns_zero_counts_not_error() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local()).unwrap();

        let result = pack
            .dispatch(
                "brain.event_counts",
                json!({
                    "since": "2020-01-01T00:00:00Z",
                    "until": "2020-01-02T00:00:00Z",
                }),
                &registry,
                &token,
            )
            .await
            .expect("empty window must not error");

        assert_eq!(result["total"], json!(0));
        assert_eq!(result["counts_by_kind"], json!({}));
        assert_eq!(result["counts_by_actor"], json!({}));
        assert_eq!(result["counts_by_verb"], json!({}));
        assert!(
            result.get("by_profile").is_none(),
            "by_profile must be omitted when no feedback events are in the window: {result}"
        );
        assert!(
            result.get("feedback_by_originating_verb").is_none(),
            "feedback origin split must be omitted when no feedback events are in the window: {result}"
        );
    }

    /// #26: with a shrunk `page_limit`, a busy `audit` kind must not crowd a
    /// quiet non-audit kind out of the unfiltered default view's shared budget.
    /// Direct unit test of `fetch_event_counts_window` (rather than the full
    /// `brain.event_counts` verb dispatch) because reproducing this at the real
    /// `MAX_WINDOW_EVENTS = 50_000` cap would require seeding tens of thousands
    /// of rows; the crowd-out mechanism is identical at any cap size.
    #[tokio::test]
    async fn audit_kind_does_not_crowd_out_non_audit_kinds_in_default_view() {
        use khive_storage::event::EventFilter;

        let (_, rt) = make_pack();
        let token = rt.authorize(Namespace::local()).unwrap();

        // 5 audit events (the busy kind) vs. 1 non-audit event (the quiet kind),
        // all in the same window. A shared, unsegregated 3-row budget ordered
        // `created_at DESC` (audit seeded last, so audit sorts first) would
        // return only audit rows and silently drop the lone `search_executed`
        // row — exactly the ~12x undercount pattern reported in #26.
        for i in 0..5 {
            seed_event(
                &rt,
                &token,
                "create",
                EventKind::Audit,
                "lambda:a",
                1_000_000 + i,
                json!({}),
            )
            .await;
        }
        seed_event(
            &rt,
            &token,
            "search",
            EventKind::SearchExecuted,
            "lambda:a",
            999_999,
            json!({}),
        )
        .await;

        let store = rt.events(&token).expect("event store");
        let base_filter = EventFilter {
            after: Some(0),
            ..EventFilter::default()
        };

        let (items, window_event_total, truncated) = crate::handlers::fetch_event_counts_window(
            store.as_ref(),
            &base_filter,
            /* unfiltered = */ true,
            /* page_limit = */ 3,
        )
        .await
        .expect("fetch_event_counts_window must succeed");

        assert_eq!(
            window_event_total, 6,
            "the true total across both segregated queries must be exact, not \
             page-limited: {items:?}"
        );
        assert!(
            truncated,
            "5 audit rows against a 3-row budget must report truncated=true"
        );
        let search_present = items.iter().any(|e| e.kind == EventKind::SearchExecuted);
        assert!(
            search_present,
            "the lone non-audit event must survive a busy-audit-kind window even \
             under a shared-looking budget: {items:?}"
        );
        let audit_count = items.iter().filter(|e| e.kind == EventKind::Audit).count();
        assert_eq!(
            audit_count, 3,
            "the audit-kind half of the budget is independently capped at \
             page_limit: {items:?}"
        );
    }

    /// #21: a small injected page size makes this exercise three storage pages
    /// while proving every page contributes to the final per-verb counts.
    #[tokio::test]
    async fn exhaustive_fetch_accumulates_counts_across_pages() {
        let (_, rt) = make_pack();
        let token = rt.authorize(Namespace::local()).unwrap();

        for i in 0..7 {
            seed_event(
                &rt,
                &token,
                "recall",
                EventKind::RecallExecuted,
                "lambda:a",
                1_000_000 + i,
                json!({}),
            )
            .await;
        }
        for i in 0..3 {
            seed_event(
                &rt,
                &token,
                "search",
                EventKind::SearchExecuted,
                "lambda:a",
                2_000_000 + i,
                json!({}),
            )
            .await;
        }

        let store = rt.events(&token).expect("event store");
        let base_filter = EventFilter {
            after: Some(0),
            ..EventFilter::default()
        };
        let (items, window_event_total, truncated) =
            crate::handlers::fetch_event_counts_window_exhaustive(
                store.as_ref(),
                &base_filter,
                /* page_size = */ 4,
                /* max_events = */ 20,
            )
            .await
            .expect("multi-page exhaustive fetch must succeed");

        let mut counts_by_verb = std::collections::BTreeMap::new();
        for event in &items {
            *counts_by_verb.entry(event.verb.as_str()).or_insert(0_u64) += 1;
        }
        assert_eq!(items.len(), 10);
        assert_eq!(window_event_total, 10);
        assert!(!truncated);
        assert_eq!(counts_by_verb.get("recall"), Some(&7));
        assert_eq!(counts_by_verb.get("search"), Some(&3));
    }

    /// The exhaustive walk pages with a strict `before` cursor; rows sharing
    /// one `created_at` microsecond across a page edge must arrive exactly
    /// once — neither dropped by the strict bound nor double-counted by the
    /// boundary re-read.
    #[tokio::test]
    async fn exhaustive_fetch_survives_timestamp_ties_across_page_edges() {
        let (_, rt) = make_pack();
        let token = rt.authorize(Namespace::local()).unwrap();

        // A tie run wider than the page size, plus one older row below it.
        for _ in 0..9 {
            seed_event(
                &rt,
                &token,
                "recall",
                EventKind::RecallExecuted,
                "lambda:a",
                5_000_000,
                json!({}),
            )
            .await;
        }
        seed_event(
            &rt,
            &token,
            "search",
            EventKind::SearchExecuted,
            "lambda:a",
            1_000_000,
            json!({}),
        )
        .await;

        let store = rt.events(&token).expect("event store");
        let base_filter = EventFilter {
            after: Some(0),
            ..EventFilter::default()
        };
        let (items, window_event_total, truncated) =
            crate::handlers::fetch_event_counts_window_exhaustive(
                store.as_ref(),
                &base_filter,
                /* page_size = */ 4,
                /* max_events = */ 20,
            )
            .await
            .expect("tie-heavy exhaustive fetch must succeed");

        assert_eq!(window_event_total, 10);
        assert_eq!(items.len(), 10, "each tied row must arrive exactly once");
        let distinct: std::collections::HashSet<uuid::Uuid> =
            items.iter().map(|event| event.id).collect();
        assert_eq!(distinct.len(), 10, "no row may be double-counted");
        assert!(!truncated);
    }

    /// A page holding MORE than one timestamp group must not cycle: the
    /// cursor steps to `oldest-in-page + 1` and the strict `created_at <`
    /// bound excludes every newer group already emitted, so the walk is
    /// monotone. Three timestamp groups with a page edge inside a group
    /// exercise the mixed-page shape directly: page one carries two groups
    /// (2×T3, 2×T2), the boundary set tracks only T2, and the next page must
    /// deliver the remaining T2 row and the T1 group exactly once each.
    #[tokio::test]
    async fn exhaustive_fetch_terminates_across_multiple_timestamp_groups_per_page() {
        let (_, rt) = make_pack();
        let token = rt.authorize(Namespace::local()).unwrap();

        // Three groups, newest first in delivery order: 2 @ T3, 3 @ T2,
        // 2 @ T1, with page_size 4 splitting the T2 group across pages.
        for _ in 0..2 {
            seed_event(
                &rt,
                &token,
                "recall",
                EventKind::RecallExecuted,
                "lambda:a",
                9_000_000,
                json!({}),
            )
            .await;
        }
        for _ in 0..3 {
            seed_event(
                &rt,
                &token,
                "search",
                EventKind::SearchExecuted,
                "lambda:a",
                5_000_000,
                json!({}),
            )
            .await;
        }
        for _ in 0..2 {
            seed_event(
                &rt,
                &token,
                "recall",
                EventKind::RecallExecuted,
                "lambda:a",
                1_000_000,
                json!({}),
            )
            .await;
        }

        let store = rt.events(&token).expect("event store");
        let base_filter = EventFilter {
            after: Some(0),
            ..EventFilter::default()
        };
        let (items, window_event_total, truncated) =
            crate::handlers::fetch_event_counts_window_exhaustive(
                store.as_ref(),
                &base_filter,
                /* page_size = */ 4,
                /* max_events = */ 20,
            )
            .await
            .expect("multi-group exhaustive fetch must terminate and succeed");

        assert_eq!(window_event_total, 7);
        assert_eq!(items.len(), 7, "every group must be reached exactly once");
        let distinct: std::collections::HashSet<uuid::Uuid> =
            items.iter().map(|event| event.id).collect();
        assert_eq!(distinct.len(), 7, "no row may be double-counted");
        let mut counts_by_ts = std::collections::BTreeMap::new();
        for event in &items {
            *counts_by_ts.entry(event.created_at).or_insert(0_u64) += 1;
        }
        assert_eq!(counts_by_ts.get(&9_000_000), Some(&2));
        assert_eq!(counts_by_ts.get(&5_000_000), Some(&3));
        assert_eq!(
            counts_by_ts.get(&1_000_000),
            Some(&2),
            "the oldest group must be reached"
        );
        assert!(!truncated);
    }

    /// Build `count` events sharing one `created_at` for bulk seeding.
    fn tied_events(namespace: &str, created_at: i64, count: usize) -> Vec<Event> {
        (0..count)
            .map(|_| {
                let mut event = Event::new(
                    namespace,
                    "recall",
                    EventKind::RecallExecuted,
                    SubstrateKind::Note,
                    "lambda:a",
                );
                event.created_at = created_at;
                event
            })
            .collect()
    }

    /// A timestamp tie that EXACTLY fills a transport-cap page is pageable:
    /// once the at-the-cap page comes back all-duplicates, the walk proves
    /// the tie complete against `count_events` and steps the strict bound to
    /// the boundary itself, reaching the older rows instead of failing. A
    /// tie WIDER than the cap must still return the typed dense-tie error.
    #[tokio::test]
    async fn exact_cap_timestamp_tie_pages_past_while_wider_tie_errors() {
        let (_, rt) = make_pack();
        let token = rt.authorize(Namespace::local()).unwrap();
        let store = rt.events(&token).expect("event store");
        let cap = usize::try_from(crate::handlers::TRANSPORT_PAGE_ROWS).unwrap();

        let mut batch = tied_events(token.namespace().as_str(), 5_000_000, cap);
        batch.extend(tied_events(token.namespace().as_str(), 1_000_000, 1));
        store.append_events(batch).await.expect("seed cap tie");

        let base_filter = EventFilter {
            after: Some(0),
            ..EventFilter::default()
        };
        let items = crate::handlers::collect_events_cursor_walk(
            store.as_ref(),
            &base_filter,
            crate::handlers::TRANSPORT_PAGE_ROWS,
            (cap as u64) + 1,
        )
        .await
        .expect("an exactly-cap tie must page past, not error");
        assert_eq!(items.len(), cap + 1, "the older row must be reached");
        let distinct: std::collections::HashSet<uuid::Uuid> =
            items.iter().map(|event| event.id).collect();
        assert_eq!(distinct.len(), cap + 1, "no row may be double-counted");

        // One more row at the tied microsecond pushes the run past the cap:
        // the walk must now fail with the typed dense-tie error, not loop.
        store
            .append_events(tied_events(token.namespace().as_str(), 5_000_000, 1))
            .await
            .expect("seed over-cap tie");
        let err = crate::handlers::collect_events_cursor_walk(
            store.as_ref(),
            &base_filter,
            crate::handlers::TRANSPORT_PAGE_ROWS,
            (cap as u64) + 2,
        )
        .await
        .expect_err("a tie wider than the cap is unpageable");
        assert!(
            err.to_string().contains("share one created_at microsecond"),
            "unexpected error: {err}"
        );
    }

    /// Delegating store that appends one prepared row to the inner store
    /// immediately AFTER serving a `count_events` call, reproducing a
    /// concurrent boundary-timestamp append landing between the walk's
    /// completeness count and its next page query.
    struct AppendAfterCountStore {
        inner: std::sync::Arc<dyn khive_storage::event::EventStore>,
        pending: tokio::sync::Mutex<Option<Event>>,
    }

    #[async_trait::async_trait]
    impl khive_storage::event::EventStore for AppendAfterCountStore {
        async fn append_event(&self, event: Event) -> khive_storage::StorageResult<()> {
            self.inner.append_event(event).await
        }
        async fn append_events(
            &self,
            events: Vec<Event>,
        ) -> khive_storage::StorageResult<khive_storage::BatchWriteSummary> {
            self.inner.append_events(events).await
        }
        async fn get_event(&self, id: uuid::Uuid) -> khive_storage::StorageResult<Option<Event>> {
            self.inner.get_event(id).await
        }
        async fn query_events(
            &self,
            filter: EventFilter,
            page: khive_storage::PageRequest,
        ) -> khive_storage::StorageResult<khive_storage::Page<Event>> {
            self.inner.query_events(filter, page).await
        }
        async fn count_events(&self, filter: EventFilter) -> khive_storage::StorageResult<u64> {
            let count = self.inner.count_events(filter).await?;
            if let Some(event) = self.pending.lock().await.take() {
                self.inner.append_event(event).await?;
            }
            Ok(count)
        }
    }

    /// The walk's at-cap completeness proof and its next page query are
    /// independent reads of a live plane: a boundary-timestamp row appended
    /// between them is a point-in-time exclusion, never an error and never a
    /// duplicate. This pins the documented read-consistency contract.
    #[tokio::test]
    async fn concurrent_boundary_append_between_count_and_next_page_is_excluded_cleanly() {
        let (_, rt) = make_pack();
        let token = rt.authorize(Namespace::local()).unwrap();
        let store = rt.events(&token).expect("event store");
        let cap = usize::try_from(crate::handlers::TRANSPORT_PAGE_ROWS).unwrap();

        let mut batch = tied_events(token.namespace().as_str(), 5_000_000, cap);
        batch.extend(tied_events(token.namespace().as_str(), 1_000_000, 1));
        store.append_events(batch).await.expect("seed cap tie");
        let racer = tied_events(token.namespace().as_str(), 5_000_000, 1)
            .pop()
            .expect("one racer event");
        let racer_id = racer.id;

        let wrapped = AppendAfterCountStore {
            inner: store.clone(),
            pending: tokio::sync::Mutex::new(Some(racer)),
        };
        let base_filter = EventFilter {
            after: Some(0),
            ..EventFilter::default()
        };
        let items = crate::handlers::collect_events_cursor_walk(
            &wrapped,
            &base_filter,
            crate::handlers::TRANSPORT_PAGE_ROWS,
            (cap as u64) + 2,
        )
        .await
        .expect("a concurrent boundary append must not error the walk");
        assert_eq!(
            items.len(),
            cap + 1,
            "the point-in-time view holds the pre-append population"
        );
        let distinct: std::collections::HashSet<uuid::Uuid> =
            items.iter().map(|event| event.id).collect();
        assert_eq!(distinct.len(), cap + 1, "no row may be double-counted");
        assert!(
            !distinct.contains(&racer_id),
            "the racing row is excluded from this walk's view, not half-included"
        );
        assert!(
            wrapped.pending.lock().await.is_none(),
            "the completeness count must have fired and injected the racing append; \
             otherwise this test exercised nothing"
        );
        assert!(
            store
                .get_event(racer_id)
                .await
                .expect("racer lookup")
                .is_some(),
            "the racing row must actually be in the store"
        );
    }

    /// Rows at `created_at == i64::MAX` admit no exclusive bound above them:
    /// the cursor must not saturate past them (which would silently skip the
    /// uncollected remainder of the group) — the walk re-reads, dedups, and
    /// still reaches every row exactly once, including the older rows.
    #[tokio::test]
    async fn max_timestamp_group_is_fully_collected_not_skipped() {
        let (_, rt) = make_pack();
        let token = rt.authorize(Namespace::local()).unwrap();
        let store = rt.events(&token).expect("event store");

        let mut batch = tied_events(token.namespace().as_str(), i64::MAX, 5);
        batch.extend(tied_events(token.namespace().as_str(), 1_000_000, 2));
        store.append_events(batch).await.expect("seed max group");

        let base_filter = EventFilter {
            after: Some(0),
            ..EventFilter::default()
        };
        let items = crate::handlers::collect_events_cursor_walk(
            store.as_ref(),
            &base_filter,
            /* page_size = */ 4,
            /* max_rows = */ 100,
        )
        .await
        .expect("the max-timestamp group must be pageable");
        assert_eq!(items.len(), 7, "all seven rows must arrive");
        let distinct: std::collections::HashSet<uuid::Uuid> =
            items.iter().map(|event| event.id).collect();
        assert_eq!(distinct.len(), 7, "no row may be double-counted");
        assert_eq!(
            items
                .iter()
                .filter(|event| event.created_at == i64::MAX)
                .count(),
            5,
            "every max-timestamp row must be collected, none skipped"
        );
    }

    /// The public handler must still identify a successful exhaustive response
    /// as exact; pagination mechanics are covered separately with injected limits.
    #[tokio::test]
    async fn exhaustive_mode_returns_exact_response_shape() {
        let (pack, rt) = make_pack_with_actor("lambda:a");
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local()).unwrap();

        seed_event(
            &rt,
            &token,
            "recall",
            EventKind::RecallExecuted,
            "lambda:a",
            1_000_000,
            json!({}),
        )
        .await;

        let result = pack
            .dispatch(
                "brain.event_counts",
                json!({"since": micros_to_iso(0), "exhaustive": true}),
                &registry,
                &token,
            )
            .await
            .expect("exhaustive brain.event_counts must succeed");

        assert_eq!(result["exhaustive"], json!(true), "got: {result}");
        assert_eq!(result["truncated"], json!(false), "got: {result}");
        assert_eq!(result["total"], json!(1), "got: {result}");
        assert_eq!(result["window_event_total"], json!(1), "got: {result}");
        assert_eq!(result["counts_by_verb"]["recall"], json!(1));
    }

    #[tokio::test]
    async fn exhaustive_fetch_rejects_windows_over_the_cap() {
        let (_, rt) = make_pack();
        let token = rt.authorize(Namespace::local()).unwrap();

        for i in 0..6 {
            seed_event(
                &rt,
                &token,
                "recall",
                EventKind::RecallExecuted,
                "lambda:a",
                1_000_000 + i,
                json!({}),
            )
            .await;
        }

        let store = rt.events(&token).expect("event store");
        let base_filter = EventFilter {
            after: Some(0),
            ..EventFilter::default()
        };
        let err = crate::handlers::fetch_event_counts_window_exhaustive(
            store.as_ref(),
            &base_filter,
            /* page_size = */ 2,
            /* max_events = */ 5,
        )
        .await
        .expect_err("an exhaustive window above the safety cap must be rejected");

        match err {
            RuntimeError::InvalidInput(msg) => {
                assert!(msg.contains("window exceeds the exhaustive limit of 5 events"));
                assert!(msg.contains("narrow `since`/`until` or add filters"));
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    /// Default (non-exhaustive) calls must keep reporting `exhaustive: false`
    /// so callers can distinguish an exact aggregate from a bounded-page one.
    #[tokio::test]
    async fn default_mode_reports_exhaustive_false() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local()).unwrap();

        let result = pack
            .dispatch(
                "brain.event_counts",
                json!({"since": micros_to_iso(0)}),
                &registry,
                &token,
            )
            .await
            .expect("brain.event_counts must succeed");

        assert_eq!(result["exhaustive"], json!(false), "got: {result}");
    }

    #[tokio::test]
    async fn kind_filter_scopes_counts() {
        let (pack, rt) = make_pack_with_actor("lambda:a");
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local()).unwrap();

        seed_event(
            &rt,
            &token,
            "search",
            EventKind::SearchExecuted,
            "lambda:a",
            1_000_000,
            json!({}),
        )
        .await;
        seed_event(
            &rt,
            &token,
            "recall",
            EventKind::RecallExecuted,
            "lambda:a",
            1_000_000,
            json!({}),
        )
        .await;

        let result = pack
            .dispatch(
                "brain.event_counts",
                json!({
                    "since": micros_to_iso(0),
                    "kind": "recall_executed",
                }),
                &registry,
                &token,
            )
            .await
            .expect("brain.event_counts must succeed");

        assert_eq!(result["total"], json!(1));
        assert_eq!(result["counts_by_kind"]["recall_executed"], json!(1));
        assert!(result["counts_by_kind"].get("search_executed").is_none());
    }

    // ── brain.mark_turn work-start marker ────────────────────

    /// `brain.mark_turn` must persist a `PhaseStarted` event carrying the
    /// calling actor, the fixed `work_class = "actor_turn"`, and the caller's
    /// free-form `label` in the `phase` field.
    #[tokio::test]
    async fn mark_turn_persists_phase_started_actor_turn_event() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local()).unwrap();

        let result = pack
            .dispatch(
                "brain.mark_turn",
                json!({ "label": "wake" }),
                &registry,
                &token,
            )
            .await
            .expect("brain.mark_turn must succeed");

        assert_eq!(result["ok"], json!(true));
        assert_eq!(result["work_class"], json!("actor_turn"));

        let page = rt
            .events(&token)
            .expect("event store")
            .query_events(
                EventFilter {
                    kinds: vec![EventKind::PhaseStarted],
                    ..EventFilter::default()
                },
                PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .expect("query_events");

        assert_eq!(
            page.items.len(),
            1,
            "exactly one PhaseStarted event must be persisted: {page:?}"
        );
        let event = &page.items[0];
        assert_eq!(event.verb, "brain.mark_turn");
        assert_eq!(
            event.actor,
            format!("{}:{}", token.actor().kind, token.actor().id)
        );
        assert_eq!(event.payload["work_class"], json!("actor_turn"));
        assert_eq!(event.payload["phase"], json!("wake"));
    }

    /// A rejected credential-shaped label must not reach the event plane.
    #[tokio::test]
    async fn mark_turn_blocks_secret_label_without_persisting_event() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local()).unwrap();
        let actor = format!("{}:{}", token.actor().kind, token.actor().id);
        let window_start = chrono::Utc::now().timestamp_micros();

        let err = pack
            .dispatch(
                "brain.mark_turn",
                json!({
                    "label": "AKIAFAKEKEY000000000", // gitleaks:allow
                }),
                &registry,
                &token,
            )
            .await
            .expect_err("credential-shaped label must be rejected");
        let window_end = chrono::Utc::now().timestamp_micros();

        assert!(
            matches!(err, RuntimeError::SecretDetected(_)),
            "credential-shaped label must return SecretDetected, got {err:?}"
        );

        let page = rt
            .events(&token)
            .expect("event store")
            .query_events(
                EventFilter {
                    kinds: vec![EventKind::PhaseStarted],
                    verbs: vec!["brain.mark_turn".to_string()],
                    actors: vec![actor],
                    after: Some(window_start.saturating_sub(1)),
                    before: Some(window_end.saturating_add(1)),
                    ..EventFilter::default()
                },
                PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .expect("query phase_started events");

        assert!(
            page.items.is_empty(),
            "rejected mark_turn must not persist phase_started events: {page:?}"
        );
    }

    /// `brain.mark_turn` calls without an explicit `label` default the
    /// `phase` field to `"actor_turn"`.
    #[tokio::test]
    async fn mark_turn_without_label_defaults_phase_to_actor_turn() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local()).unwrap();

        pack.dispatch("brain.mark_turn", json!({}), &registry, &token)
            .await
            .expect("brain.mark_turn must succeed with no params");

        let page = rt
            .events(&token)
            .expect("event store")
            .query_events(
                EventFilter {
                    kinds: vec![EventKind::PhaseStarted],
                    ..EventFilter::default()
                },
                PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .expect("query_events");

        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].payload["phase"], json!("actor_turn"));
    }

    /// The `actor_turn` work-unit marker must be visible in
    /// `brain.event_counts`'s `counts_by_work_class` (the flywheel-scorecard
    /// discipline-ratio denominator) and grouped correctly by actor via the
    /// existing kind-agnostic `counts_by_actor` machinery.
    #[tokio::test]
    async fn mark_turn_counted_by_work_class_and_grouped_by_actor_in_event_counts() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local()).unwrap();

        pack.dispatch("brain.mark_turn", json!({}), &registry, &token)
            .await
            .expect("brain.mark_turn 1");
        pack.dispatch("brain.mark_turn", json!({}), &registry, &token)
            .await
            .expect("brain.mark_turn 2");

        let result = pack
            .dispatch(
                "brain.event_counts",
                json!({ "since": micros_to_iso(0) }),
                &registry,
                &token,
            )
            .await
            .expect("brain.event_counts must succeed");

        assert_eq!(result["counts_by_work_class"]["actor_turn"], json!(2));
        // Default caller scope uses the canonical actor id, not the stored kind:id spelling.
        let actor = &token.actor().id;
        assert_eq!(result["counts_by_actor"][actor.as_str()], json!(2));
        assert_eq!(result["counts_by_kind"]["phase_started"], json!(2));
    }

    #[tokio::test]
    async fn actor_filter_scopes_counts() {
        let (pack, rt) = make_pack_with_read_scope(Some("lambda:a"), &["lambda:b"], &[]);
        let registry = empty_registry();
        let token = rt
            .authorize_with_visibility(Namespace::local(), rt.visible_namespaces().to_vec())
            .unwrap();

        seed_event(
            &rt,
            &token,
            "search",
            EventKind::SearchExecuted,
            "lambda:a",
            1_000_000,
            json!({}),
        )
        .await;
        seed_event(
            &rt,
            &token,
            "search",
            EventKind::SearchExecuted,
            "lambda:b",
            1_000_000,
            json!({}),
        )
        .await;

        let result = pack
            .dispatch(
                "brain.event_counts",
                json!({
                    "since": micros_to_iso(0),
                    "actor": "lambda:b",
                }),
                &registry,
                &token,
            )
            .await
            .expect("brain.event_counts must succeed");

        assert_eq!(result["total"], json!(1));
        assert_eq!(result["counts_by_actor"]["lambda:b"], json!(1));
        assert!(result["counts_by_actor"].get("lambda:a").is_none());
    }

    /// #943: stored actor strings are prefixed (`actor:<kind>:<id>`), but the
    /// verb's own param doc example is the bare seat form (`"lambda:khive"`).
    /// The bare form must match the prefixed stored form, and the prefixed
    /// form must keep matching exactly as before.
    #[tokio::test]
    async fn actor_filter_matches_bare_and_prefixed_spelling() {
        let (pack, rt) = make_pack_with_read_scope(Some("lambda:riley"), &["lambda:khive"], &[]);
        let registry = empty_registry();
        let token = rt
            .authorize_with_visibility(Namespace::local(), rt.visible_namespaces().to_vec())
            .unwrap();

        seed_event(
            &rt,
            &token,
            "search",
            EventKind::SearchExecuted,
            "actor:lambda:riley",
            1_000_000,
            json!({}),
        )
        .await;
        seed_event(
            &rt,
            &token,
            "search",
            EventKind::SearchExecuted,
            "actor:lambda:khive",
            1_000_000,
            json!({}),
        )
        .await;

        // Bare seat spelling must match the prefixed stored form.
        let bare = pack
            .dispatch(
                "brain.event_counts",
                json!({
                    "since": micros_to_iso(0),
                    "actor": "lambda:riley",
                }),
                &registry,
                &token,
            )
            .await
            .expect("brain.event_counts must succeed");
        assert_eq!(
            bare["total"],
            json!(1),
            "bare seat spelling must match the actor:-prefixed stored form: {bare}"
        );
        assert_eq!(bare["counts_by_actor"]["actor:lambda:riley"], json!(1));
        assert!(bare["counts_by_actor"].get("actor:lambda:khive").is_none());

        // Already-prefixed spelling must keep matching exactly (no false widening).
        let prefixed = pack
            .dispatch(
                "brain.event_counts",
                json!({
                    "since": micros_to_iso(0),
                    "actor": "actor:lambda:khive",
                }),
                &registry,
                &token,
            )
            .await
            .expect("brain.event_counts must succeed");
        assert_eq!(
            prefixed["total"],
            json!(1),
            "already-prefixed actor spelling must keep exact-match behavior: {prefixed}"
        );
        assert_eq!(prefixed["counts_by_actor"]["actor:lambda:khive"], json!(1));
        assert!(prefixed["counts_by_actor"]
            .get("actor:lambda:riley")
            .is_none());
    }

    /// #943: `counts_by_verb` aggregates `event.verb` over the page with the
    /// same truncation semantics as `counts_by_kind`/`counts_by_actor`.
    #[tokio::test]
    async fn counts_by_verb_aggregates_per_verb() {
        let (pack, rt) = make_pack_with_actor("lambda:a");
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local()).unwrap();

        seed_event(
            &rt,
            &token,
            "search",
            EventKind::SearchExecuted,
            "lambda:a",
            1_000_000,
            json!({}),
        )
        .await;
        seed_event(
            &rt,
            &token,
            "search",
            EventKind::SearchExecuted,
            "lambda:a",
            1_000_000,
            json!({}),
        )
        .await;
        seed_event(
            &rt,
            &token,
            "recall",
            EventKind::RecallExecuted,
            "lambda:a",
            1_000_000,
            json!({}),
        )
        .await;

        let result = pack
            .dispatch(
                "brain.event_counts",
                json!({"since": micros_to_iso(0)}),
                &registry,
                &token,
            )
            .await
            .expect("brain.event_counts must succeed");

        assert_eq!(result["total"], json!(3));
        assert_eq!(result["counts_by_verb"]["search"], json!(2));
        assert_eq!(result["counts_by_verb"]["recall"], json!(1));
    }

    #[tokio::test]
    async fn feedback_events_split_by_served_by_profile_id() {
        let (pack, rt) = make_pack_with_actor("lambda:a");
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local()).unwrap();

        seed_event(
            &rt,
            &token,
            "brain.feedback",
            EventKind::FeedbackExplicit,
            "lambda:a",
            1_000_000,
            json!({"served_by_profile_id": "balanced-recall-v1", "signal": "useful"}),
        )
        .await;
        seed_event(
            &rt,
            &token,
            "brain.feedback",
            EventKind::FeedbackExplicit,
            "lambda:a",
            1_000_000,
            json!({"served_by_profile_id": "balanced-recall-v1", "signal": "wrong"}),
        )
        .await;
        // Missing served_by_profile_id — must fall into the "unspecified" bucket
        // rather than being dropped from the split.
        seed_event(
            &rt,
            &token,
            "brain.feedback",
            EventKind::FeedbackExplicit,
            "lambda:a",
            1_000_000,
            json!({"signal": "useful"}),
        )
        .await;

        let result = pack
            .dispatch(
                "brain.event_counts",
                json!({"since": micros_to_iso(0)}),
                &registry,
                &token,
            )
            .await
            .expect("brain.event_counts must succeed");

        assert_eq!(result["total"], json!(3));
        assert_eq!(
            result["by_profile"]["balanced-recall-v1"],
            json!(2),
            "got: {result}"
        );
        assert_eq!(
            result["by_profile"]["unspecified"],
            json!(1),
            "got: {result}"
        );
        assert_eq!(
            result["feedback_by_originating_verb"]["brain.feedback"],
            json!(3),
            "legacy rows without an origin marker must fall back to event.verb: {result}"
        );
    }

    /// #34: `counts_by_signal` (flat) and `by_profile_and_signal` (crossed with
    /// `served_by_profile_id`) let a caller compute per-profile negative-signal share
    /// in one call, mirroring the profile-only split above.
    #[tokio::test]
    async fn feedback_events_split_by_signal_and_crossed_with_profile() {
        let (pack, rt) = make_pack_with_actor("lambda:a");
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local()).unwrap();

        seed_event(
            &rt,
            &token,
            "brain.feedback",
            EventKind::FeedbackExplicit,
            "lambda:a",
            1_000_000,
            json!({"served_by_profile_id": "balanced-recall-v1", "signal": "useful"}),
        )
        .await;
        seed_event(
            &rt,
            &token,
            "brain.feedback",
            EventKind::FeedbackExplicit,
            "lambda:a",
            1_000_000,
            json!({"served_by_profile_id": "balanced-recall-v1", "signal": "wrong"}),
        )
        .await;
        seed_event(
            &rt,
            &token,
            "brain.feedback",
            EventKind::FeedbackExplicit,
            "lambda:a",
            1_000_000,
            json!({"served_by_profile_id": "khive-recall-v1", "signal": "wrong"}),
        )
        .await;
        // No `signal` field at all — must fall into "unspecified", same
        // no-drop convention as the missing-profile case above.
        seed_event(
            &rt,
            &token,
            "brain.feedback",
            EventKind::FeedbackExplicit,
            "lambda:a",
            1_000_000,
            json!({"served_by_profile_id": "balanced-recall-v1"}),
        )
        .await;

        let result = pack
            .dispatch(
                "brain.event_counts",
                json!({"since": micros_to_iso(0)}),
                &registry,
                &token,
            )
            .await
            .expect("brain.event_counts must succeed");

        assert_eq!(result["total"], json!(4));
        assert_eq!(
            result["counts_by_signal"]["useful"],
            json!(1),
            "got: {result}"
        );
        assert_eq!(
            result["counts_by_signal"]["wrong"],
            json!(2),
            "got: {result}"
        );
        assert_eq!(
            result["counts_by_signal"]["unspecified"],
            json!(1),
            "got: {result}"
        );
        assert_eq!(
            result["by_profile_and_signal"]["balanced-recall-v1"]["useful"],
            json!(1),
            "got: {result}"
        );
        assert_eq!(
            result["by_profile_and_signal"]["balanced-recall-v1"]["wrong"],
            json!(1),
            "got: {result}"
        );
        assert_eq!(
            result["by_profile_and_signal"]["balanced-recall-v1"]["unspecified"],
            json!(1),
            "got: {result}"
        );
        assert_eq!(
            result["by_profile_and_signal"]["khive-recall-v1"]["wrong"],
            json!(1),
            "got: {result}"
        );
    }

    #[tokio::test]
    async fn invalid_kind_lists_valid_values() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local()).unwrap();

        let err = pack
            .dispatch(
                "brain.event_counts",
                json!({"since": micros_to_iso(0), "kind": "not_a_real_kind"}),
                &registry,
                &token,
            )
            .await
            .expect_err("unknown kind must be rejected");

        match &err {
            RuntimeError::InvalidInput(msg) => {
                assert!(
                    msg.contains("not_a_real_kind"),
                    "error must name the rejected value: {msg}"
                );
                assert!(
                    msg.contains("recall_executed") || msg.contains("Valid:"),
                    "error must list valid EventKind values: {msg}"
                );
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_since_datetime_names_the_field() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local()).unwrap();

        let err = pack
            .dispatch(
                "brain.event_counts",
                json!({"since": "not-a-date"}),
                &registry,
                &token,
            )
            .await
            .expect_err("malformed since must be rejected");

        match &err {
            RuntimeError::InvalidInput(msg) => {
                assert!(
                    msg.contains("since") && msg.contains("not-a-date"),
                    "error must name the field and offending value: {msg}"
                );
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    /// A date-only `since` (no time-of-day component) is accepted rather than
    /// silently resolving to a null result (#984), and anchors to the named
    /// day's earliest instant in the CONFIGURED DISPLAY TIMEZONE (ADR-169 D1)
    /// — not midnight UTC. New York's 2026-07-07 begins at 04:00Z, so the
    /// two rules produce different instants and a UTC-anchoring regression
    /// fails this assert.
    #[tokio::test]
    async fn date_only_since_anchors_in_the_display_timezone() {
        let (pack, rt) = make_pack_with_tz("America/New_York");
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local()).unwrap();

        let result = pack
            .dispatch(
                "brain.event_counts",
                json!({"since": "2026-07-07"}),
                &registry,
                &token,
            )
            .await
            .expect("date-only `since` must be accepted, not silently nulled");

        let expected_us = chrono::DateTime::parse_from_rfc3339("2026-07-07T00:00:00-04:00")
            .unwrap()
            .timestamp_micros();
        let since = result["since"].as_str().expect("since must be a string");
        assert_eq!(
            since,
            micros_to_iso(expected_us),
            "date-only `since` must anchor to the day's earliest instant in the \
             display timezone (04:00Z for New York), not midnight UTC: {result}"
        );
    }

    /// With the display timezone configured as UTC, date-only anchoring is
    /// exactly the old midnight-UTC behavior — the control arm separating
    /// "anchors in the configured zone" from "anchors somewhere new".
    #[tokio::test]
    async fn date_only_since_under_utc_config_is_midnight_utc() {
        let (pack, rt) = make_pack_with_tz("UTC");
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local()).unwrap();

        let result = pack
            .dispatch(
                "brain.event_counts",
                json!({"since": "2026-07-07"}),
                &registry,
                &token,
            )
            .await
            .expect("date-only `since` must be accepted under UTC config");

        let since = result["since"].as_str().expect("since must be a string");
        assert!(
            since.starts_with("2026-07-07T00:00:00"),
            "UTC display timezone anchors date-only `since` at midnight UTC: {result}"
        );
    }

    /// On a zone's spring-forward date local midnight does not exist; the
    /// anchor must be the least instant carrying the date (Havana jumps
    /// 00:00 -> 01:00 on 2021-03-14, so the day begins at 01:00 local,
    /// 05:00Z), never a silent fall-back to UTC midnight and never an error.
    #[tokio::test]
    async fn date_only_since_on_a_gap_date_takes_the_least_instant() {
        let (pack, rt) = make_pack_with_tz("America/Havana");
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local()).unwrap();

        let result = pack
            .dispatch(
                "brain.event_counts",
                json!({"since": "2021-03-14"}),
                &registry,
                &token,
            )
            .await
            .expect("a gap date must anchor, not error");

        let boundary_us = chrono::DateTime::parse_from_rfc3339("2021-03-14T01:00:00-04:00")
            .unwrap()
            .timestamp_micros();
        let since = result["since"].as_str().expect("since must be a string");
        let since_us = chrono::DateTime::parse_from_rfc3339(&since.replace("Z", "+00:00"))
            .expect("since must parse")
            .timestamp_micros();
        // The shared anchor resolves gap dates by bisection at one-second
        // granularity (see `khive_runtime::time_anchor`), so the result is
        // within [boundary, boundary + 1s) — never before the boundary
        // (which would be the previous local date) and never the silent
        // UTC-midnight (04:00Z) the old rule produced.
        assert!(
            (0..1_000_000).contains(&(since_us - boundary_us)),
            "the gap date anchors at its 01:00-local/05:00Z boundary \
             (within the 1s bisection granularity): {result}"
        );
    }

    /// A date the zone's local calendar skips entirely (Samoa crossed the
    /// date line over 2011-12-30) has no instant to anchor to: the verb
    /// rejects it with an error naming the zone, rather than silently
    /// anchoring somewhere else.
    #[tokio::test]
    async fn date_only_since_on_a_skipped_date_is_rejected_naming_the_zone() {
        let (pack, rt) = make_pack_with_tz("Pacific/Apia");
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local()).unwrap();

        let result = pack
            .dispatch(
                "brain.event_counts",
                json!({"since": "2011-12-30"}),
                &registry,
                &token,
            )
            .await;

        match result {
            Err(RuntimeError::InvalidInput(msg)) => {
                assert!(
                    msg.contains("since") && msg.contains("Pacific/Apia"),
                    "the error must name the field and the zone: {msg}"
                );
            }
            other => panic!("expected InvalidInput for a skipped date, got {other:?}"),
        }
    }

    /// A date-only `until` must include the entire named day in the display
    /// timezone, not just its first instant — the window is half-open
    /// `[since, until)`, so a date-only `until` has to roll to the *next*
    /// day's earliest instant or the whole named day is silently dropped
    /// (#994). Pinned to America/New_York: the named day 2026-07-07 spans
    /// [04:00Z on the 7th, 04:00Z on the 8th), so every edge below sits four
    /// hours away from where UTC anchoring would put it — a UTC regression
    /// flips the included/excluded status of the edge events.
    #[tokio::test]
    async fn date_only_until_includes_the_whole_named_day() {
        let (pack, rt) = make_pack_with_tz("America/New_York");
        let registry = empty_registry();
        let (_, actor_rt) = make_pack_with_actor("lambda:a");
        let token = actor_rt.authorize(Namespace::local()).unwrap();

        let us = |s: &str| {
            chrono::DateTime::parse_from_rfc3339(s)
                .expect("fixture instant")
                .timestamp_micros()
        };
        // The window is [2026-07-06T04:00:00Z, 2026-07-08T04:00:00Z).
        // Four seeds pin all four edge behaviors:
        let before_window = us("2026-07-06T03:59:59Z"); // excluded (before since)
        let at_since = us("2026-07-06T04:00:00Z"); // included (since inclusive)
        let end_of_named_day = us("2026-07-08T03:59:59.999999Z"); // included edge
        let next_day_start = us("2026-07-08T04:00:00Z"); // excluded (until exclusive)

        for ts in [before_window, at_since, end_of_named_day, next_day_start] {
            seed_event(
                &rt,
                &token,
                "search",
                EventKind::SearchExecuted,
                "lambda:a",
                ts,
                json!({}),
            )
            .await;
        }

        assert_eq!(seeded_count(&rt, &token).await, 4);

        let result = pack
            .dispatch(
                "brain.event_counts",
                json!({
                    "since": "2026-07-06",
                    "until": "2026-07-07",
                }),
                &registry,
                &token,
            )
            .await
            .expect("brain.event_counts must succeed");

        assert_eq!(
            result["total"],
            json!(2),
            "the window must include exactly the since edge and the named day's \
             last instant, excluding the instants at both outer edges: {result}"
        );
    }

    /// A value that is neither a valid date nor a valid RFC-3339 datetime
    /// still gets a named validation error, never a null result.
    #[tokio::test]
    async fn garbage_since_is_rejected_not_silently_nulled() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local()).unwrap();

        let result = pack
            .dispatch(
                "brain.event_counts",
                json!({"since": "2026-13-99"}),
                &registry,
                &token,
            )
            .await;

        match result {
            Ok(v) => panic!("invalid `since` must not silently succeed with a value: {v}"),
            Err(RuntimeError::InvalidInput(msg)) => {
                assert!(
                    msg.contains("since") && msg.contains("2026-13-99"),
                    "error must name the field and offending value: {msg}"
                );
            }
            Err(other) => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    /// A value that is neither a valid date nor a valid RFC-3339 datetime
    /// still gets a named validation error on `until`, not silently nulled —
    /// mirrors `garbage_since_is_rejected_not_silently_nulled` for the
    /// role-aware `until` parse path (#994).
    #[tokio::test]
    async fn garbage_until_is_rejected_not_silently_nulled() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local()).unwrap();

        let result = pack
            .dispatch(
                "brain.event_counts",
                json!({"since": "2026-07-01", "until": "2026-13-40"}),
                &registry,
                &token,
            )
            .await;

        match result {
            Ok(v) => panic!("invalid `until` must not silently succeed with a value: {v}"),
            Err(RuntimeError::InvalidInput(msg)) => {
                assert!(
                    msg.contains("until") && msg.contains("2026-13-40"),
                    "error must name the field and offending value: {msg}"
                );
            }
            Err(other) => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    /// `until` at chrono's maximum representable date (NaiveDate::MAX,
    /// 262142-12-31) would overflow the next-day roll; the handler must return
    /// a named validation error instead of panicking on caller input (#994).
    #[tokio::test]
    async fn max_date_only_until_is_rejected_not_panicked() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local()).unwrap();

        let result = pack
            .dispatch(
                "brain.event_counts",
                json!({"since": "2026-07-01", "until": "+262142-12-31"}),
                &registry,
                &token,
            )
            .await;

        match result {
            Ok(v) => panic!("`until` at the max representable date must not silently succeed: {v}"),
            Err(RuntimeError::InvalidInput(msg)) => {
                assert!(
                    msg.contains("until") && msg.contains("262142-12-31"),
                    "error must name the field and offending value: {msg}"
                );
            }
            Err(other) => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_since_is_rejected() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local()).unwrap();

        let err = pack
            .dispatch("brain.event_counts", json!({}), &registry, &token)
            .await
            .expect_err("since is required");

        match &err {
            RuntimeError::InvalidInput(msg) => {
                assert!(
                    msg.contains("since"),
                    "error must name the missing field: {msg}"
                );
                assert!(
                    msg.to_ascii_lowercase().contains("iso-8601")
                        || msg.to_ascii_lowercase().contains("rfc-3339")
                        || msg.to_ascii_lowercase().contains("rfc3339"),
                    "error must name the expected format: {msg}"
                );
                assert!(
                    msg.contains("2026-07-01T00:00:00Z") || msg.contains("T00:00:00Z"),
                    "error must include an example datetime: {msg}"
                );
            }
            other => panic!("expected InvalidInput, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn work_class_split_reads_phase_payload_not_resource_fallback() {
        let (pack, rt) = make_pack_with_actor("lambda:khive");
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local()).unwrap();

        // Today's real shape: work_class lives directly on the Phase* payload
        // (khive_storage::telemetry::PhaseStartedPayload / PhaseCompletedPayload).
        seed_event(
            &rt,
            &token,
            "ann_warm",
            EventKind::PhaseStarted,
            "lambda:khive",
            1_000_000,
            json!({"work_class": "warm", "phase": "ann_warm", "corpus_size": 553_000}),
        )
        .await;
        seed_event(
            &rt,
            &token,
            "ann_warm",
            EventKind::PhaseCompleted,
            "lambda:khive",
            1_000_000,
            json!({"work_class": "warm", "phase": "ann_warm", "wall_us": 41_000_000, "cpu_us": null}),
        )
        .await;
        // A different work_class, distinct kind, so the split can't be faked by
        // just echoing counts_by_kind.
        seed_event(
            &rt,
            &token,
            "ann_warm",
            EventKind::PhaseStarted,
            "lambda:khive",
            1_000_000,
            json!({"work_class": "maintenance", "phase": "index_rebuild"}),
        )
        .await;
        // A non-phase event with no work_class anywhere — must not appear in the split.
        seed_event(
            &rt,
            &token,
            "search",
            EventKind::SearchExecuted,
            "lambda:khive",
            1_000_000,
            json!({}),
        )
        .await;
        // Future shape: work_class nested under `resource`, no top-level `work_class`.
        // Must be picked up via the fallback path, not just the phase path.
        seed_event(
            &rt,
            &token,
            "recall",
            EventKind::RecallExecuted,
            "lambda:khive",
            1_000_000,
            json!({"resource": {"work_class": "interactive"}}),
        )
        .await;
        // Both paths present: phase path must win (checked first).
        seed_event(
            &rt,
            &token,
            "ann_warm",
            EventKind::PhaseStarted,
            "lambda:khive",
            1_000_000,
            json!({"work_class": "warm", "resource": {"work_class": "maintenance"}}),
        )
        .await;

        let result = pack
            .dispatch(
                "brain.event_counts",
                json!({"since": micros_to_iso(0)}),
                &registry,
                &token,
            )
            .await
            .expect("brain.event_counts must succeed");

        assert_eq!(result["total"], json!(6), "got: {result}");
        assert_eq!(
            result["counts_by_work_class"]["warm"],
            json!(3),
            "phase-path warm rows (2 pure + 1 dual-path, phase wins) must all count as warm: {result}"
        );
        assert_eq!(
            result["counts_by_work_class"]["maintenance"],
            json!(1),
            "the dual-path row must count under warm (phase path), not maintenance \
             (resource fallback) — phase path must be checked first: {result}"
        );
        assert_eq!(
            result["counts_by_work_class"]["interactive"],
            json!(1),
            "resource.work_class fallback must be read when no top-level work_class exists: {result}"
        );
        // search_executed carried no work_class anywhere -> excluded from the split,
        // not zero-filled, so the split total (5) is less than the overall total (6).
        let split_total: u64 = result["counts_by_work_class"]
            .as_object()
            .expect("counts_by_work_class must be an object")
            .values()
            .map(|v| v.as_u64().unwrap())
            .sum();
        assert_eq!(
            split_total, 5,
            "events with neither work_class path present must be excluded, not counted: {result}"
        );
    }

    #[tokio::test]
    async fn work_class_split_absent_when_no_event_carries_it() {
        let (pack, rt) = make_pack_with_actor("lambda:a");
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local()).unwrap();

        seed_event(
            &rt,
            &token,
            "search",
            EventKind::SearchExecuted,
            "lambda:a",
            1_000_000,
            json!({}),
        )
        .await;

        let result = pack
            .dispatch(
                "brain.event_counts",
                json!({"since": micros_to_iso(0)}),
                &registry,
                &token,
            )
            .await
            .expect("brain.event_counts must succeed");

        assert!(
            result.get("counts_by_work_class").is_none(),
            "counts_by_work_class must be omitted, not an empty object, when no event in \
             the window carries a work_class: {result}"
        );
    }

    /// ADR-103 Amendment 1 (PR #927): the dispatch-boundary audit-row emitter
    /// now stamps `resource: {"work_class": "interactive"}` on EVERY audit
    /// row outcome, denied and errored included -- only `resource.cost_unit`
    /// is scoped to a successful dispatch (ADR-103 Decision (a) requires
    /// `work_class` on every event; Amendment 1's omission rule covers
    /// `cost_unit` alone). This pins that a failed and a denied dispatch's
    /// audit rows are still counted in `counts_by_work_class` via the same
    /// `payload.resource.work_class` fallback path other non-phase events
    /// use: outcome plays no role in the split, only payload shape does.
    #[tokio::test]
    async fn failed_and_denied_audit_rows_count_via_resource_work_class_fallback() {
        let (pack, rt) = make_pack();
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local()).unwrap();

        let mut errored = Event::new(
            token.namespace().as_str(),
            "probe",
            EventKind::Audit,
            SubstrateKind::Event,
            "lambda:khive",
        );
        errored.created_at = 1_000_000;
        errored.outcome = khive_types::EventOutcome::Error;
        errored.payload = json!({"resource": {"work_class": "interactive"}});
        rt.events(&token)
            .expect("event store")
            .append_event(errored)
            .await
            .expect("seed errored audit event");

        let mut denied = Event::new(
            token.namespace().as_str(),
            "list",
            EventKind::Audit,
            SubstrateKind::Event,
            "lambda:khive",
        );
        denied.created_at = 1_000_000;
        denied.outcome = khive_types::EventOutcome::Denied;
        denied.payload = json!({"resource": {"work_class": "interactive"}});
        rt.events(&token)
            .expect("event store")
            .append_event(denied)
            .await
            .expect("seed denied audit event");

        let result = pack
            .dispatch(
                "brain.event_counts",
                json!({"since": micros_to_iso(0)}),
                &registry,
                &token,
            )
            .await
            .expect("brain.event_counts must succeed");

        assert_eq!(result["total"], json!(2), "got: {result}");
        assert_eq!(
            result["counts_by_work_class"]["interactive"],
            json!(2),
            "a failed dispatch and a denied dispatch must both still count under \
             counts_by_work_class -- work_class is stamped on every event outcome \
             (ADR-103 Decision (a)), unlike cost_unit which is success-only: {result}"
        );
    }

    /// Events with no `payload.resource.cost_unit` anywhere in the window (pre-#927
    /// events, or events like Phase* spans that never carry `cost_unit` at all) must
    /// not synthesize `total_cost_unit` / `cost_unit_by_verb` out of thin air — both
    /// keys must be entirely absent, not zero-filled.
    #[tokio::test]
    async fn cost_unit_fields_absent_when_no_event_carries_cost_unit() {
        let (pack, rt) = make_pack_with_actor("lambda:khive");
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local()).unwrap();

        seed_event(
            &rt,
            &token,
            "ann_warm",
            EventKind::PhaseCompleted,
            "lambda:khive",
            1_000_000,
            json!({"work_class": "warm", "phase": "ann_warm", "wall_us": 1, "cpu_us": null}),
        )
        .await;
        // Pre-#927 audit row: work_class present, no cost_unit at all.
        seed_event(
            &rt,
            &token,
            "search",
            EventKind::Audit,
            "lambda:khive",
            1_000_000,
            json!({"resource": {"work_class": "interactive"}}),
        )
        .await;

        let result = pack
            .dispatch(
                "brain.event_counts",
                json!({"since": micros_to_iso(0)}),
                &registry,
                &token,
            )
            .await
            .expect("brain.event_counts must succeed");

        assert!(
            result.get("total_cost_unit").is_none(),
            "total_cost_unit must be omitted, not zero, when no event carries cost_unit: {result}"
        );
        assert!(
            result.get("cost_unit_by_verb").is_none(),
            "cost_unit_by_verb must be omitted, not an empty object, when no event carries \
             cost_unit: {result}"
        );
    }

    /// ADR-103 Amendment 1 / PR #927: `payload.resource.cost_unit` on successful
    /// dispatch audit rows must sum into `total_cost_unit` and split per-verb into
    /// `cost_unit_by_verb`, following the same aggregation style as
    /// `counts_by_kind`/`counts_by_actor`/`counts_by_work_class`.
    #[tokio::test]
    async fn cost_unit_sums_total_and_per_verb() {
        let (pack, rt) = make_pack_with_read_scope(Some("lambda:a"), &[], &["lambda:a"]);
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local()).unwrap();

        seed_event(
            &rt,
            &token,
            "create",
            EventKind::Audit,
            "lambda:a",
            1_000_000,
            json!({"resource": {"work_class": "interactive", "cost_unit": 4}}),
        )
        .await;
        seed_event(
            &rt,
            &token,
            "create",
            EventKind::Audit,
            "lambda:a",
            1_000_000,
            json!({"resource": {"work_class": "interactive", "cost_unit": 6}}),
        )
        .await;
        seed_event(
            &rt,
            &token,
            "stats",
            EventKind::Audit,
            "lambda:b",
            1_000_000,
            json!({"resource": {"work_class": "interactive", "cost_unit": 1}}),
        )
        .await;
        // No cost_unit at all — must not contribute to either sum.
        seed_event(
            &rt,
            &token,
            "search",
            EventKind::SearchExecuted,
            "lambda:a",
            1_000_000,
            json!({}),
        )
        .await;
        // Errored dispatch: work_class present, cost_unit absent per Amendment 1's
        // "absence has exactly two meanings" rule — must not contribute.
        let mut errored = Event::new(
            token.namespace().as_str(),
            "create",
            EventKind::Audit,
            SubstrateKind::Event,
            "lambda:a",
        );
        errored.created_at = 1_000_000;
        errored.outcome = khive_types::EventOutcome::Error;
        errored.payload = json!({"resource": {"work_class": "interactive"}});
        rt.events(&token)
            .expect("event store")
            .append_event(errored)
            .await
            .expect("seed errored audit event");

        let result = pack
            .dispatch(
                "brain.event_counts",
                json!({"since": micros_to_iso(0), "all_actors": true}),
                &registry,
                &token,
            )
            .await
            .expect("brain.event_counts must succeed");

        assert_eq!(result["total"], json!(5), "got: {result}");
        assert_eq!(
            result["total_cost_unit"],
            json!(11),
            "total_cost_unit must sum only the events carrying cost_unit (4+6+1): {result}"
        );
        assert_eq!(
            result["cost_unit_by_verb"]["create"],
            json!(10),
            "got: {result}"
        );
        assert_eq!(
            result["cost_unit_by_verb"]["stats"],
            json!(1),
            "got: {result}"
        );
        assert!(
            result["cost_unit_by_verb"].get("search").is_none(),
            "a verb whose events carry no cost_unit must not appear in the split: {result}"
        );
    }

    /// The window filter (`since`/`until`) must apply to `cost_unit` aggregation
    /// exactly like every other count field — an event outside the window must not
    /// contribute to `total_cost_unit`.
    #[tokio::test]
    async fn cost_unit_respects_window_filter() {
        let (pack, rt) = make_pack_with_actor("lambda:a");
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local()).unwrap();

        seed_event(
            &rt,
            &token,
            "create",
            EventKind::Audit,
            "lambda:a",
            1_500_000,
            json!({"resource": {"work_class": "interactive", "cost_unit": 9}}),
        )
        .await;
        // Outside the window entirely.
        seed_event(
            &rt,
            &token,
            "create",
            EventKind::Audit,
            "lambda:a",
            5_000_000,
            json!({"resource": {"work_class": "interactive", "cost_unit": 100}}),
        )
        .await;

        let result = pack
            .dispatch(
                "brain.event_counts",
                json!({
                    "since": micros_to_iso(1_000_000),
                    "until": micros_to_iso(2_000_000),
                }),
                &registry,
                &token,
            )
            .await
            .expect("brain.event_counts must succeed");

        assert_eq!(result["total"], json!(1), "got: {result}");
        assert_eq!(result["total_cost_unit"], json!(9), "got: {result}");
    }

    /// Mirrors `cost_unit.rs`'s own overflow-clamp tests: the sum must saturate
    /// rather than panic when individual `cost_unit` values are extreme.
    #[tokio::test]
    async fn cost_unit_total_saturates_on_overflow_instead_of_panicking() {
        let (pack, rt) = make_pack_with_actor("lambda:a");
        let registry = empty_registry();
        let token = rt.authorize(Namespace::local()).unwrap();

        seed_event(
            &rt,
            &token,
            "knowledge.index",
            EventKind::Audit,
            "lambda:a",
            1_000_000,
            json!({"resource": {"work_class": "interactive", "cost_unit": i64::MAX}}),
        )
        .await;
        seed_event(
            &rt,
            &token,
            "knowledge.index",
            EventKind::Audit,
            "lambda:a",
            1_000_000,
            json!({"resource": {"work_class": "interactive", "cost_unit": 5}}),
        )
        .await;

        let result = pack
            .dispatch(
                "brain.event_counts",
                json!({"since": micros_to_iso(0)}),
                &registry,
                &token,
            )
            .await
            .expect("brain.event_counts must succeed");

        assert_eq!(result["total_cost_unit"], json!(i64::MAX), "got: {result}");
        assert_eq!(
            result["cost_unit_by_verb"]["knowledge.index"],
            json!(i64::MAX),
            "got: {result}"
        );
    }

    #[test]
    fn handler_is_public_verb_with_required_since() {
        let h = crate::handlers::BRAIN_HANDLERS
            .iter()
            .find(|h| h.name == "brain.event_counts")
            .expect("brain.event_counts must be registered");
        assert_eq!(h.visibility, khive_types::Visibility::Verb);
        assert!(
            h.params.iter().any(|p| p.name == "since" && p.required),
            "since must be documented as required"
        );
        assert!(
            h.params.iter().any(|p| p.name == "until" && !p.required),
            "until must be documented as optional"
        );
    }

    /// Issue #14: the untruncated case is unaffected -- `total`/
    /// `total_cost_unit` keep their existing bare names. This is the shape
    /// every other test in this module already exercises via the real
    /// handler (window sizes here never approach `MAX_WINDOW_EVENTS`).
    #[test]
    fn truncatable_total_key_is_unchanged_when_not_truncated() {
        assert_eq!(BrainPack::truncatable_total_key("total", false), "total");
        assert_eq!(
            BrainPack::truncatable_total_key("total_cost_unit", false),
            "total_cost_unit"
        );
    }

    /// Issue #14: when the window was truncated, the misleading bare key
    /// must never be emitted -- it is renamed to an explicitly
    /// page-scoped/incomplete key instead, so a caller reading `total`
    /// directly gets nothing rather than a number that may be off by an
    /// order of magnitude.
    #[test]
    fn truncatable_total_key_is_renamed_when_truncated() {
        assert_eq!(
            BrainPack::truncatable_total_key("total", true),
            "total_page_scoped"
        );
        assert_eq!(
            BrainPack::truncatable_total_key("total_cost_unit", true),
            "total_cost_unit_page_scoped"
        );
    }
}

#[path = "read_scope_tests.rs"]
mod read_scope_tests;

/// Anonymous explicit judgments must not train a shared default or a named
/// profile. Implicit signals and caller abstention retain their admission rules.
#[path = "anonymous_feedback_admission_tests.rs"]
mod anonymous_feedback_admission_tests;

/// #808: `brain.feedback`'s `FeedbackExplicit` event must carry the resolved
/// caller identity (ADR-096 per-request identity), not the hardcoded literal
/// `"brain"` it used to stamp regardless of who called it.
#[path = "feedback_actor_tests.rs"]
mod feedback_actor_tests;

/// The dispatch-signal counters, and the laziness they exist to measure.
///
/// A profiler can say the process spends its time serializing snapshots. It
/// cannot say whether that is one serialization per signal or a thousand, and
/// the remedy differs. These arms pin what each counter counts, including the
/// two paths that deliberately count nothing, and then pin that a run of
/// signals serializes nothing until someone reads.
#[path = "dispatch_counters_tests.rs"]
mod dispatch_counters;

#[path = "event_usage_tests.rs"]
mod event_usage_tests;

#[cfg(test)]
#[path = "tests_presented_recall.rs"]
mod presented_recall_tests;

//! Replay routing: replaying the event log must leave every profile in the
//! state the live path left it in, and a recall hit or a note access must not
//! credit any entity posterior.

use std::collections::BTreeMap;

use khive_brain_core::{
    BalancedRecallState, BrainSignal, BrainState, ProfileLifecycle, ServeAttribution,
};
use khive_storage::event::Event;
use khive_types::{EventKind, SubstrateKind};
use uuid::Uuid;

use super::{interpret, replay_events};

const NAMESPACE: &str = "local";
const DEFAULT_PROFILE: &str = "balanced-recall-v1";
const PROFILE_A: &str = "named-a-v1";
const PROFILE_B: &str = "named-b-v1";
const ARCHIVED_PROFILE: &str = "archived-v1";
const ENTITY_CAPACITY: usize = 8;

fn target(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

/// How the live path folded an event.
enum Live {
    /// Folded by the dispatch hook through `apply_dispatch_signal`.
    Dispatch,
    /// Folded by the feedback write path into the profile that served it.
    Feedback(&'static str),
    /// Not folded into any profile.
    Nothing,
}

/// An explicit judgment and the profile that owns it.
struct Judgment {
    profile: &'static str,
    target: Uuid,
    useful: bool,
}

/// What the test knows about one event by construction.
struct Meta {
    live: Live,
    /// Whether the live rule folds this event into exactly one profile.
    folded: bool,
    judgment: Option<Judgment>,
}

/// An event log together with what is known about each row.
#[derive(Default)]
struct Fixture {
    events: Vec<Event>,
    meta: Vec<Meta>,
}

fn attribution_label(signal: &BrainSignal) -> &'static str {
    match signal {
        BrainSignal::RecallHit {
            serve_attribution, ..
        } => match serve_attribution {
            ServeAttribution::Profile => "profile",
            ServeAttribution::Unspecified => "unspecified",
            ServeAttribution::Unattributed => "unattributed",
        },
        _ => "not a recall hit",
    }
}

impl Fixture {
    fn push(&mut self, event: Event, meta: Meta) {
        self.events.push(event);
        self.meta.push(meta);
    }

    /// Explicit judgment feedback. `stamp` is the serving profile recorded on
    /// the row; `None` is a row written before that field existed, which the
    /// live path had credited to the default profile.
    fn feedback(&mut self, target_id: Uuid, signal: &str, stamp: Option<&'static str>) {
        let mut event = Event::new(
            NAMESPACE,
            "brain.feedback",
            EventKind::FeedbackExplicit,
            SubstrateKind::Event,
            "brain",
        );
        event.target_id = Some(target_id);
        event.payload = serde_json::json!({ "signal": signal });
        if let Some(profile) = stamp {
            event.payload["served_by_profile_id"] = serde_json::json!(profile);
        }
        assert!(
            matches!(interpret(&event), BrainSignal::Feedback { .. }),
            "{signal} must interpret as plain judgment feedback"
        );
        let profile = stamp.unwrap_or(DEFAULT_PROFILE);
        self.push(
            event,
            Meta {
                live: Live::Feedback(profile),
                folded: true,
                judgment: Some(Judgment {
                    profile,
                    target: target_id,
                    useful: signal == "useful",
                }),
            },
        );
    }

    /// A successful recall that hit `target_id`. `expected` is the attribution
    /// class the row must interpret as; `folded` is whether the live rule folds
    /// a hit of that class.
    fn recall(
        &mut self,
        target_id: Uuid,
        marker: Option<&str>,
        served_by: Option<&str>,
        expected: &str,
        folded: bool,
    ) {
        let mut event = Event::new(
            NAMESPACE,
            "memory.recall",
            EventKind::RecallExecuted,
            SubstrateKind::Note,
            "memory",
        );
        event.target_id = Some(target_id);
        event.payload = serde_json::json!({});
        if let Some(marker) = marker {
            event.payload["serve_attribution"] = serde_json::json!(marker);
        }
        if let Some(profile) = served_by {
            event.payload["served_by_profile_id"] = serde_json::json!(profile);
        }
        assert_eq!(attribution_label(&interpret(&event)), expected);
        self.push(
            event,
            Meta {
                live: Live::Dispatch,
                folded,
                judgment: None,
            },
        );
    }

    fn note(&mut self, target_id: Uuid) {
        let mut event = Event::new(
            NAMESPACE,
            "get",
            EventKind::Audit,
            SubstrateKind::Note,
            "memory",
        );
        event.target_id = Some(target_id);
        assert!(matches!(
            interpret(&event),
            BrainSignal::NoteAccessed { .. }
        ));
        self.push(
            event,
            Meta {
                live: Live::Dispatch,
                folded: true,
                judgment: None,
            },
        );
    }

    /// Feedback telemetry that carries no judgment. It names a serving profile
    /// so that folding it anywhere would show.
    fn unjudged_feedback(&mut self, target_id: Uuid) {
        let mut event = Event::new(
            NAMESPACE,
            "brain.feedback",
            EventKind::FeedbackUnjudged,
            SubstrateKind::Event,
            "brain",
        );
        event.target_id = Some(target_id);
        event.payload = serde_json::json!({
            "signal": "useful",
            "served_by_profile_id": PROFILE_A,
        });
        self.push(
            event,
            Meta {
                live: Live::Nothing,
                folded: false,
                judgment: None,
            },
        );
    }

    /// Explicit judgment feedback for which no serving profile was resolved.
    /// The write path stamps `served_by_profile_id: null` on such a row and
    /// folds it into no posterior.
    fn unresolved_feedback(&mut self, target_id: Uuid) {
        let mut event = Event::new(
            NAMESPACE,
            "brain.feedback",
            EventKind::FeedbackExplicit,
            SubstrateKind::Event,
            "brain",
        );
        event.target_id = Some(target_id);
        event.payload = serde_json::json!({
            "signal": "useful",
            "served_by_profile_id": null,
        });
        assert!(
            matches!(interpret(&event), BrainSignal::Feedback { .. }),
            "an unresolved row must still interpret as judgment feedback"
        );
        self.push(
            event,
            Meta {
                live: Live::Nothing,
                folded: false,
                judgment: None,
            },
        );
    }

    /// A section feedback row. It changes section state only.
    fn section_feedback(&mut self) {
        let mut event = Event::new(
            NAMESPACE,
            "brain.section_feedback",
            EventKind::FeedbackExplicit,
            SubstrateKind::Event,
            "brain",
        );
        event.payload = serde_json::json!({
            "served_by_profile_id": DEFAULT_PROFILE,
            "section_signals": { "overview": "useful" },
        });
        self.push(
            event,
            Meta {
                live: Live::Nothing,
                folded: false,
                judgment: None,
            },
        );
    }
}

/// A namespace with the default profile, two active named profiles and one
/// archived named profile, each with its own state.
fn fixture_state() -> BrainState {
    let mut state = BrainState::new(ENTITY_CAPACITY);
    for (id, lifecycle) in [
        (PROFILE_A, None),
        (PROFILE_B, None),
        (ARCHIVED_PROFILE, Some(ProfileLifecycle::Archived)),
    ] {
        let mut record = state.profiles[DEFAULT_PROFILE].clone();
        record.id = id.into();
        if let Some(lifecycle) = lifecycle {
            record.lifecycle = lifecycle;
        }
        state.profiles.insert(record.id.clone(), record);
        state
            .profile_states
            .insert(id.into(), BalancedRecallState::new(ENTITY_CAPACITY));
    }
    state
}

/// Mixed log: judgments for the default and two named profiles, recall hits of
/// every attribution class, a hit naming an archived profile, a note access, an
/// unjudged feedback row, a judgment row with no resolved serving profile and a
/// section feedback row.
fn mixed_fixture() -> Fixture {
    let mut fx = Fixture::default();
    fx.feedback(target(1), "useful", Some(DEFAULT_PROFILE));
    fx.feedback(target(2), "not_useful", None);
    fx.feedback(target(3), "useful", Some(PROFILE_A));
    fx.feedback(target(3), "wrong", Some(PROFILE_A));
    fx.feedback(target(4), "not_useful", Some(PROFILE_B));
    fx.feedback(target(3), "useful", Some(PROFILE_B));
    fx.recall(target(1), Some("profile"), Some(PROFILE_A), "profile", true);
    fx.recall(target(5), Some("unspecified"), None, "unspecified", true);
    fx.recall(target(6), None, None, "unspecified", true);
    fx.recall(target(7), Some("unattributed"), None, "unattributed", false);
    fx.recall(
        target(8),
        Some("profile"),
        Some(ARCHIVED_PROFILE),
        "profile",
        false,
    );
    fx.recall(target(2), None, Some(PROFILE_B), "profile", true);
    fx.note(target(1));
    fx.unjudged_feedback(target(9));
    fx.unresolved_feedback(target(10));
    fx.section_feedback();
    fx
}

/// Log whose every row is folded into exactly one profile.
fn foldable_fixture() -> Fixture {
    let mut fx = Fixture::default();
    fx.feedback(target(1), "useful", Some(DEFAULT_PROFILE));
    fx.feedback(target(2), "not_useful", None);
    fx.feedback(target(3), "useful", Some(PROFILE_A));
    fx.feedback(target(4), "wrong", Some(PROFILE_B));
    fx.recall(target(1), Some("profile"), Some(PROFILE_A), "profile", true);
    fx.recall(target(5), Some("unspecified"), None, "unspecified", true);
    fx.note(target(1));
    fx
}

/// Fold the log into `state` through the functions the live path folds with:
/// the dispatch hook's and the feedback write path's.
fn apply_live(state: &mut BrainState, fx: &Fixture) {
    for (event, meta) in fx.events.iter().zip(&fx.meta) {
        match meta.live {
            Live::Nothing => {}
            Live::Dispatch => {
                let signal = interpret(event);
                crate::apply_dispatch_signal(state, &signal);
            }
            Live::Feedback(profile_id) => {
                let signal = interpret(event);
                crate::apply_feedback_signal(state, profile_id, &signal);
            }
        }
    }
}

/// Event count stored on each profile record.
fn record_counts(state: &BrainState) -> BTreeMap<String, u64> {
    state
        .profiles
        .iter()
        .map(|(id, record)| (id.clone(), record.total_events))
        .collect()
}

/// Serialized snapshot of the default profile and of every named profile state.
fn snapshots(state: &BrainState) -> BTreeMap<String, serde_json::Value> {
    let mut out = BTreeMap::new();
    out.insert(
        DEFAULT_PROFILE.to_owned(),
        serde_json::to_value(state.balanced_recall.to_snapshot())
            .expect("serialize default profile snapshot"),
    );
    for (id, profile_state) in &state.profile_states {
        out.insert(
            id.clone(),
            serde_json::to_value(profile_state.to_snapshot())
                .expect("serialize named profile snapshot"),
        );
    }
    out
}

fn event_count(state: &BalancedRecallState) -> usize {
    usize::try_from(state.total_events).expect("event count fits in usize")
}

/// Events folded into the default profile and every named profile together.
fn folded_total(state: &BrainState) -> usize {
    event_count(&state.balanced_recall)
        + state
            .profile_states
            .values()
            .map(event_count)
            .sum::<usize>()
}

/// Entity posteriors as `(alpha, beta)` pairs keyed by entity.
fn entity_map(state: &BalancedRecallState) -> BTreeMap<Uuid, (f64, f64)> {
    state
        .entity_posteriors
        .to_snapshot()
        .iter()
        .map(|(id, p)| (*id, (p.alpha(), p.beta())))
        .collect()
}

/// Entity posteriors a profile holds after seeing only its own judgments:
/// every entity starts at Beta(1, 1), a useful judgment adds 1 to alpha and any
/// other explicit judgment adds 1 to beta.
fn expected_entities(fx: &Fixture, profile: &str) -> BTreeMap<Uuid, (f64, f64)> {
    let mut expected = BTreeMap::new();
    for judgment in fx
        .meta
        .iter()
        .filter_map(|meta| meta.judgment.as_ref())
        .filter(|judgment| judgment.profile == profile)
    {
        let (alpha, beta) = expected.entry(judgment.target).or_insert((1.0, 1.0));
        if judgment.useful {
            *alpha += 1.0;
        } else {
            *beta += 1.0;
        }
    }
    expected
}

#[test]
fn replay_matches_live_routing_per_profile() {
    let fx = mixed_fixture();

    // The rows marked as not folded are the ones the live rule skips.
    let reference = fixture_state();
    for (event, meta) in fx.events.iter().zip(&fx.meta) {
        if matches!(meta.live, Live::Dispatch) {
            let signal = interpret(event);
            let routed = crate::dispatch_route(&reference, &signal) != crate::FoldRoute::Skip;
            assert_eq!(routed, meta.folded, "live routing of {}", event.verb);
        }
    }

    let mut live = fixture_state();
    apply_live(&mut live, &fx);

    let mut replayed = fixture_state();
    replay_events(&mut replayed, &fx.events);

    let live_snapshots = snapshots(&live);
    let replayed_snapshots = snapshots(&replayed);
    assert_eq!(
        live_snapshots.keys().collect::<Vec<_>>(),
        replayed_snapshots.keys().collect::<Vec<_>>()
    );
    for (profile, live_snapshot) in &live_snapshots {
        assert_eq!(
            live_snapshot, &replayed_snapshots[profile],
            "replayed state of {profile} differs from the live state"
        );
    }
    assert_eq!(
        record_counts(&live),
        record_counts(&replayed),
        "replayed profile records differ from the live records"
    );

    // The comparison is not vacuous: each active profile received events and
    // the archived profile received none.
    assert!(event_count(&replayed.balanced_recall) > 0);
    assert!(event_count(&replayed.profile_states[PROFILE_A]) > 0);
    assert!(event_count(&replayed.profile_states[PROFILE_B]) > 0);
    assert_eq!(event_count(&replayed.profile_states[ARCHIVED_PROFILE]), 0);
}

/// Judgments credited to a profile while it was active replay into that
/// profile after it is archived: the judgment rule applies no lifecycle check,
/// so replay from an archived state matches the live fold.
#[test]
fn replay_credits_judgments_to_a_profile_archived_after_them() {
    let mut fx = Fixture::default();
    fx.feedback(target(1), "useful", Some(ARCHIVED_PROFILE));
    fx.feedback(target(2), "wrong", Some(ARCHIVED_PROFILE));

    // Live: the profile is active while it is credited, then archived.
    let mut live = fixture_state();
    live.profiles
        .get_mut(ARCHIVED_PROFILE)
        .expect("fixture profile")
        .lifecycle = ProfileLifecycle::Active;
    apply_live(&mut live, &fx);
    live.profiles
        .get_mut(ARCHIVED_PROFILE)
        .expect("fixture profile")
        .lifecycle = ProfileLifecycle::Archived;

    // Replay: the profile is archived in the state replay starts from.
    let mut replayed = fixture_state();
    replay_events(&mut replayed, &fx.events);

    assert_eq!(snapshots(&live), snapshots(&replayed));
    assert_eq!(record_counts(&live), record_counts(&replayed));
    assert_eq!(event_count(&replayed.profile_states[ARCHIVED_PROFILE]), 2);
    assert_eq!(event_count(&replayed.balanced_recall), 0);
}

#[test]
fn replay_named_profile_entity_evidence_matches_own_judgments() {
    let fx = mixed_fixture();
    let mut replayed = fixture_state();
    replay_events(&mut replayed, &fx.events);

    for profile in [PROFILE_A, PROFILE_B] {
        let expected = expected_entities(&fx, profile);
        assert!(!expected.is_empty());
        assert_eq!(
            entity_map(&replayed.profile_states[profile]),
            expected,
            "entity posteriors of {profile}"
        );
    }

    // The expected values are the judgments of each profile alone: the useful
    // and wrong judgments of one entity in profile A, and one judgment per
    // entity in profile B. The recall hits naming these profiles add nothing.
    assert_eq!(
        expected_entities(&fx, PROFILE_A),
        BTreeMap::from([(target(3), (2.0, 2.0))])
    );
    assert_eq!(
        expected_entities(&fx, PROFILE_B),
        BTreeMap::from([(target(3), (2.0, 1.0)), (target(4), (1.0, 2.0))])
    );

    // The default profile holds only the two default-served judgments.
    assert_eq!(
        entity_map(&replayed.balanced_recall),
        expected_entities(&fx, DEFAULT_PROFILE)
    );
}

#[test]
fn replay_balanced_only_fixture_matches_prior_reducer() {
    // Default-served explicit judgments only: some stamped with the default
    // profile, some written before the stamp existed. All fold into the default
    // profile, so the result is fixed by the reducer arithmetic alone.
    //
    // Start values: salience Beta(2, 8); relevance Beta(7, 3); temporal
    // Beta(1, 9); every entity Beta(1, 1).
    //
    // Per event: a useful judgment adds 1 to the salience alpha and to the
    // target's entity alpha; a not_useful or wrong judgment adds 1 to the
    // salience beta and to the target's entity beta. Relevance and temporal do
    // not move, and every event adds 1 to total_events.
    //
    //   1. target 1 useful      salience (3, 8)   entity 1 (2, 1)
    //   2. target 1 useful      salience (4, 8)   entity 1 (3, 1)
    //   3. target 2 not_useful  salience (4, 9)   entity 2 (1, 2)
    //   4. target 3 wrong       salience (4, 10)  entity 3 (1, 2)
    //   5. target 1 not_useful  salience (4, 11)  entity 1 (3, 2)
    let mut fx = Fixture::default();
    fx.feedback(target(1), "useful", Some(DEFAULT_PROFILE));
    fx.feedback(target(1), "useful", None);
    fx.feedback(target(2), "not_useful", Some(DEFAULT_PROFILE));
    fx.feedback(target(3), "wrong", None);
    fx.feedback(target(1), "not_useful", Some(DEFAULT_PROFILE));

    let mut replayed = fixture_state();
    replay_events(&mut replayed, &fx.events);

    let balanced = &replayed.balanced_recall;
    assert_eq!(
        (balanced.salience.alpha(), balanced.salience.beta()),
        (4.0, 11.0)
    );
    assert_eq!(
        (balanced.relevance.alpha(), balanced.relevance.beta()),
        (7.0, 3.0)
    );
    assert_eq!(
        (balanced.temporal.alpha(), balanced.temporal.beta()),
        (1.0, 9.0)
    );
    assert_eq!(event_count(balanced), 5);
    assert_eq!(
        entity_map(balanced),
        BTreeMap::from([
            (target(1), (3.0, 2.0)),
            (target(2), (1.0, 2.0)),
            (target(3), (1.0, 2.0)),
        ])
    );

    // No named profile received anything.
    for profile_state in replayed.profile_states.values() {
        assert_eq!(event_count(profile_state), 0);
        assert!(profile_state.entity_posteriors.is_empty());
    }
}

#[test]
fn replay_folded_event_counts_sum_to_log_length() {
    // (a) Every row is folded into exactly one profile, so the events counted
    // across the default profile and all named profiles equal the log length.
    let foldable = foldable_fixture();
    assert!(foldable.meta.iter().all(|meta| meta.folded));
    let mut state = fixture_state();
    let before = folded_total(&state);
    assert_eq!(before, 0);
    replay_events(&mut state, &foldable.events);
    assert_eq!(foldable.events.len(), 7);
    assert_eq!(folded_total(&state) - before, foldable.events.len());

    // (b) In the mixed log, five rows are folded into no profile: the
    // unattributed hit, the hit naming the archived profile, the unjudged
    // feedback row, the judgment row with no resolved serving profile and the
    // section feedback row. Every other row is counted once, in the profile
    // the live rule chose.
    let mixed = mixed_fixture();
    let not_folded = mixed.meta.iter().filter(|meta| !meta.folded).count();
    assert_eq!(mixed.events.len(), 16);
    assert_eq!(not_folded, 5);
    let mut state = fixture_state();
    let before = folded_total(&state);
    replay_events(&mut state, &mixed.events);
    assert_eq!(
        folded_total(&state) - before,
        mixed.events.len() - not_folded
    );

    // Per profile: default 5 (two judgments, two default-routed hits, one note
    // access), profile A 3 (two judgments, one hit), profile B 3 (two
    // judgments, one hit), archived profile 0.
    assert_eq!(event_count(&state.balanced_recall), 5);
    assert_eq!(event_count(&state.profile_states[PROFILE_A]), 3);
    assert_eq!(event_count(&state.profile_states[PROFILE_B]), 3);
    assert_eq!(event_count(&state.profile_states[ARCHIVED_PROFILE]), 0);
}

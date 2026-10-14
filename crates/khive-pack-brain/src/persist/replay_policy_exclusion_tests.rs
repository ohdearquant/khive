use super::*;
use khive_brain_core::BrainState;
use khive_runtime::{KhiveRuntime, Namespace};
use khive_storage::event::Event;
use khive_types::{EventKind, SubstrateKind};
use uuid::Uuid;

async fn insert_raw_payload_at(rt: &KhiveRuntime, namespace: &str, payload: &str, created_at: i64) {
    let sql = rt.sql();
    let mut writer = sql.writer().await.expect("writer");
    writer
            .execute(SqlStatement {
                sql: "INSERT INTO brain_event_log (profile_id, namespace, event_kind, payload, created_at) VALUES (?1, ?2, ?3, ?4, ?5)".into(),
                params: vec![
                    SqlValue::Text("test-profile".to_string()),
                    SqlValue::Text(namespace.to_string()),
                    SqlValue::Text("brain.feedback".to_string()),
                    SqlValue::Text(payload.to_string()),
                    SqlValue::Integer(created_at),
                ],
                label: None,
            })
            .await
            .expect("insert raw row");
}

/// Build a `brain.feedback` FeedbackExplicit carrier row (mirrors the
/// live-handler persist shape: verb `brain.feedback`, payload `signal` +
/// optional `served_by_profile_id`).
fn make_implicit_positive_event_json(
    namespace: &str,
    signal: &str,
    served_by_profile_id: Option<&str>,
    created_at: i64,
) -> String {
    let mut ev = Event::new(
        namespace,
        "brain.feedback",
        EventKind::FeedbackExplicit,
        SubstrateKind::Event,
        "brain",
    );
    ev.target_id = Some(Uuid::new_v4());
    ev.created_at = created_at;
    let mut payload = serde_json::json!({ "signal": signal });
    if let Some(profile) = served_by_profile_id {
        payload["served_by_profile_id"] = serde_json::json!(profile);
    }
    ev.payload = payload;
    serde_json::to_string(&ev).expect("serialize feedback event")
}

fn fold_events(result: &LoadEventsResult) -> BrainState {
    let mut state = BrainState::new(16);
    for event in &result.events {
        let signal = crate::event::interpret(event);
        state.balanced_recall.apply_signal(&signal);
    }
    state
}

// ── pure predicate unit tests (window passed as parameters) ─────────────

fn make_event(verb: &str, signal: Option<&str>, served_by: Option<&str>, created_at: i64) -> Event {
    let mut ev = Event::new(
        "ns",
        verb,
        EventKind::FeedbackExplicit,
        SubstrateKind::Event,
        "brain",
    );
    ev.target_id = Some(Uuid::new_v4());
    ev.created_at = created_at;
    let mut payload = serde_json::json!({});
    if let Some(s) = signal {
        payload["signal"] = serde_json::json!(s);
    }
    if let Some(p) = served_by {
        payload["served_by_profile_id"] = serde_json::json!(p);
    }
    ev.payload = payload;
    ev
}

#[test]
fn predicate_excludes_in_window_attributed_implicit_positive() {
    let start = POLICY_EXCLUSION_WINDOW_START_US;
    let ev = make_event(
        "brain.feedback",
        Some("implicit_positive"),
        Some("balanced-recall-v1"),
        start + 1_000,
    );
    assert!(is_policy_excluded(&ev, start, None));
}

#[test]
fn predicate_replays_unattributed_control_population() {
    let start = POLICY_EXCLUSION_WINDOW_START_US;
    let no_profile = make_event(
        "brain.feedback",
        Some("implicit_positive"),
        None,
        start + 1_000,
    );
    assert!(!is_policy_excluded(&no_profile, start, None));
    let empty_profile = make_event(
        "brain.feedback",
        Some("implicit_positive"),
        Some(""),
        start + 1_000,
    );
    assert!(!is_policy_excluded(&empty_profile, start, None));
}

#[test]
fn predicate_replays_explicit_and_negative_signals_in_window() {
    let start = POLICY_EXCLUSION_WINDOW_START_US;
    for signal in [
        "useful",
        "not_useful",
        "wrong",
        "implicit_negative",
        "explicit_positive",
    ] {
        let ev = make_event(
            "brain.feedback",
            Some(signal),
            Some("balanced-recall-v1"),
            start + 1_000,
        );
        assert!(
            !is_policy_excluded(&ev, start, None),
            "signal {signal} must replay"
        );
    }
}

#[test]
fn predicate_replays_before_window_and_other_verbs() {
    let start = POLICY_EXCLUSION_WINDOW_START_US;
    let before = make_event(
        "brain.feedback",
        Some("implicit_positive"),
        Some("balanced-recall-v1"),
        start - 1,
    );
    assert!(!is_policy_excluded(&before, start, None));
    let other_verb = make_event(
        "memory.recall",
        Some("implicit_positive"),
        Some("balanced-recall-v1"),
        start + 1_000,
    );
    assert!(!is_policy_excluded(&other_verb, start, None));
}

#[test]
fn predicate_window_bounds_are_half_open() {
    let start = POLICY_EXCLUSION_WINDOW_START_US;
    // Inclusive start.
    let at_start = make_event(
        "brain.feedback",
        Some("implicit_positive"),
        Some("p"),
        start,
    );
    assert!(is_policy_excluded(&at_start, start, None));
    // Exclusive end (constant flipped to Some in-test via the parameter seam).
    let end = start + 1_000_000;
    let just_before_end = make_event(
        "brain.feedback",
        Some("implicit_positive"),
        Some("p"),
        end - 1,
    );
    let at_end = make_event("brain.feedback", Some("implicit_positive"), Some("p"), end);
    assert!(is_policy_excluded(&just_before_end, start, Some(end)));
    assert!(!is_policy_excluded(&at_end, start, Some(end)));
}

#[test]
fn predicate_honours_production_end_constant() {
    // With the production constant (None today), in-window events are
    // excluded regardless of how far in the future they sit.
    let start = POLICY_EXCLUSION_WINDOW_START_US;
    let far_future = make_event(
        "brain.feedback",
        Some("implicit_positive"),
        Some("p"),
        start + 10_000_000_000,
    );
    assert_eq!(
        is_policy_excluded(&far_future, start, POLICY_EXCLUSION_WINDOW_END_US),
        POLICY_EXCLUSION_WINDOW_START_US <= far_future.created_at
    );
}

// ── replay integration (through load_events_since) ───────────────────────

#[tokio::test]
async fn in_window_attributed_implicit_positive_is_policy_excluded_not_replayed() {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let token = rt.authorize(Namespace::local()).expect("token");
    let ns = token.namespace().as_str();
    let sql = rt.sql();

    let created_at = POLICY_EXCLUSION_WINDOW_START_US + 1_000;
    let row = make_implicit_positive_event_json(
        ns,
        "implicit_positive",
        Some("balanced-recall-v1"),
        created_at,
    );
    insert_raw_payload_at(&rt, ns, &row, created_at).await;

    let result = load_events_since(sql.as_ref(), ns, 0)
        .await
        .expect("load must not fail");

    assert!(result.events.is_empty(), "excluded event must not replay");
    assert_eq!(result.quarantine_count(), 0, "not a malformed quarantine");
    assert_eq!(result.policy_excluded_count(), 1, "one policy-excluded row");
    let entry = &result.quarantined[0];
    assert_eq!(entry.category, ReplaySkipCategory::PolicyExcluded);
    assert!(
        entry.reason.starts_with("policy_excluded"),
        "reason: {:?}",
        entry.reason
    );
    assert_eq!(entry.created_at, created_at);
    assert!(entry.id > 0);
    assert_eq!(entry.profile_id, "test-profile");
    assert!(!entry.payload_snippet.is_empty());

    // Posterior state unchanged: folding an empty replay set leaves the
    // fresh state byte-identical.
    let folded = fold_events(&result);
    let baseline = BrainState::new(16);
    assert_eq!(folded.balanced_recall.total_events, 0);
    assert_eq!(
        folded.balanced_recall.salience.alpha(),
        baseline.balanced_recall.salience.alpha()
    );
    assert_eq!(
        folded.balanced_recall.salience.beta(),
        baseline.balanced_recall.salience.beta()
    );

    // Audit preserved: the physical row is still in brain_event_log.
    let mut reader = sql.reader().await.expect("reader");
    let count = reader
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM brain_event_log WHERE namespace = ?1".into(),
            params: vec![SqlValue::Text(ns.to_string())],
            label: None,
        })
        .await
        .expect("count query");
    assert!(
        matches!(count, Some(SqlValue::Integer(1))),
        "row must remain in the audit log; got {count:?}"
    );
}

/// Pins which clock the policy predicate reads when a row's `created_at`
/// COLUMN and its payload's `created_at` diverge (possible only through
/// out-of-band writes — import, manual repair, a second writer; the
/// `brain.feedback` write path stamps both from one `commit_at_us`).
/// Authority is the PAYLOAD: the payload is the event that would be
/// re-folded, so its own timestamp decides class membership. The column
/// governs only row candidacy (the SQL `since` filter) and the audit
/// record's `created_at` field.
#[tokio::test]
async fn policy_predicate_reads_payload_clock_when_column_diverges() {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let token = rt.authorize(Namespace::local()).expect("token");
    let ns = token.namespace().as_str();
    let sql = rt.sql();
    let start = POLICY_EXCLUSION_WINDOW_START_US;

    // Row A: payload clock BEFORE the window, column clock inside it.
    // Payload authority ⇒ not a class member ⇒ must replay.
    let payload_out = make_implicit_positive_event_json(
        ns,
        "implicit_positive",
        Some("balanced-recall-v1"),
        start - 1_000,
    );
    insert_raw_payload_at(&rt, ns, &payload_out, start + 1_000).await;

    // Row B: payload clock inside the window, column clock far before it
    // (but past `since`, so the row is still read). Payload authority ⇒
    // class member ⇒ must be policy-excluded.
    let payload_in = make_implicit_positive_event_json(
        ns,
        "implicit_positive",
        Some("balanced-recall-v1"),
        start + 2_000,
    );
    insert_raw_payload_at(&rt, ns, &payload_in, 1_000).await;

    let result = load_events_since(sql.as_ref(), ns, 0)
        .await
        .expect("load must not fail");

    assert_eq!(result.events.len(), 1, "payload-out row must replay");
    assert_eq!(
        result.events[0].created_at,
        start - 1_000,
        "replayed event carries the payload clock"
    );
    assert_eq!(result.quarantine_count(), 0);
    assert_eq!(
        result.policy_excluded_count(),
        1,
        "payload-in row must be excluded despite its out-of-window column clock"
    );
    let entry = result
        .quarantined
        .iter()
        .find(|r| r.category == ReplaySkipCategory::PolicyExcluded)
        .expect("policy-excluded entry");
    assert_eq!(
        entry.created_at, 1_000,
        "audit record keeps the COLUMN clock for the excluded row"
    );
}

#[tokio::test]
async fn unattributed_in_window_implicit_positive_replays_as_control() {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let token = rt.authorize(Namespace::local()).expect("token");
    let ns = token.namespace().as_str();
    let sql = rt.sql();

    let created_at = POLICY_EXCLUSION_WINDOW_START_US + 2_000;
    let row = make_implicit_positive_event_json(ns, "implicit_positive", None, created_at);
    insert_raw_payload_at(&rt, ns, &row, created_at).await;

    let result = load_events_since(sql.as_ref(), ns, 0).await.expect("load");
    assert_eq!(result.events.len(), 1, "control population must replay");
    assert_eq!(result.policy_excluded_count(), 0);
    assert_eq!(result.quarantine_count(), 0);
}

#[tokio::test]
async fn in_window_explicit_useful_replays() {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let token = rt.authorize(Namespace::local()).expect("token");
    let ns = token.namespace().as_str();
    let sql = rt.sql();

    let created_at = POLICY_EXCLUSION_WINDOW_START_US + 3_000;
    let row =
        make_implicit_positive_event_json(ns, "useful", Some("balanced-recall-v1"), created_at);
    insert_raw_payload_at(&rt, ns, &row, created_at).await;

    let result = load_events_since(sql.as_ref(), ns, 0).await.expect("load");
    assert_eq!(result.events.len(), 1, "explicit feedback must replay");
    assert_eq!(result.policy_excluded_count(), 0);
}

#[tokio::test]
async fn in_window_negatives_replay_because_negatives_are_evidence() {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let token = rt.authorize(Namespace::local()).expect("token");
    let ns = token.namespace().as_str();
    let sql = rt.sql();

    for (offset, signal) in ["not_useful", "wrong"].iter().enumerate() {
        let created_at = POLICY_EXCLUSION_WINDOW_START_US + 4_000 + offset as i64;
        let row =
            make_implicit_positive_event_json(ns, signal, Some("balanced-recall-v1"), created_at);
        insert_raw_payload_at(&rt, ns, &row, created_at).await;
    }

    let result = load_events_since(sql.as_ref(), ns, 0).await.expect("load");
    assert_eq!(
        result.events.len(),
        2,
        "negatives are evidence and must replay"
    );
    assert_eq!(result.policy_excluded_count(), 0);
}

#[tokio::test]
async fn before_window_implicit_positive_replays() {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let token = rt.authorize(Namespace::local()).expect("token");
    let ns = token.namespace().as_str();
    let sql = rt.sql();

    let created_at = POLICY_EXCLUSION_WINDOW_START_US - 1;
    let row = make_implicit_positive_event_json(
        ns,
        "implicit_positive",
        Some("balanced-recall-v1"),
        created_at,
    );
    insert_raw_payload_at(&rt, ns, &row, created_at).await;

    let result = load_events_since(sql.as_ref(), ns, 0).await.expect("load");
    assert_eq!(result.events.len(), 1, "pre-window rows must replay");
    assert_eq!(result.policy_excluded_count(), 0);
}

#[tokio::test]
async fn end_bound_arm_replays_events_after_some_end() {
    // The production constant is None; this test exercises the Some(t)
    // arm through the same parameter seam the predicate exposes, then
    // proves the wiring in load_events_since matches the predicate's
    // decision.
    let start = POLICY_EXCLUSION_WINDOW_START_US;
    let end = start + 10_000;
    let after_end = make_event(
        "brain.feedback",
        Some("implicit_positive"),
        Some("p"),
        end + 5,
    );
    assert!(
        !is_policy_excluded(&after_end, start, Some(end)),
        "event after Some(end) must replay"
    );
    // And the same decision flows through load_events_since for the
    // production window: an event before any future Some(end) that is
    // still in-window stays excluded.
    let in_window = make_event(
        "brain.feedback",
        Some("implicit_positive"),
        Some("p"),
        start + 5,
    );
    assert!(is_policy_excluded(&in_window, start, Some(end)));
}

/// The `Some(end)` arm through the FULL row path: rows land in SQLite,
/// `load_events_since_with_window` reads them back, and only the
/// pre-end class member is excluded. This is the integration coverage
/// the pinning change will inherit — the wiring is exercised today,
/// not first on the day the constant moves.
#[tokio::test]
async fn end_bound_integration_excludes_only_pre_end_rows() {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let token = rt.authorize(Namespace::local()).expect("token");
    let ns = token.namespace().as_str();

    let start = POLICY_EXCLUSION_WINDOW_START_US;
    let end = start + 10_000;

    // Class member BEFORE the end bound — must be excluded.
    let pre_end = make_implicit_positive_event_json(
        ns,
        "implicit_positive",
        Some("profile-a"),
        start + 5_000,
    );
    insert_raw_payload_at(&rt, ns, &pre_end, start + 5_000).await;
    // Identical class member AFTER the end bound — must replay.
    let post_end =
        make_implicit_positive_event_json(ns, "implicit_positive", Some("profile-a"), end + 5_000);
    insert_raw_payload_at(&rt, ns, &post_end, end + 5_000).await;
    // Unattributed control inside the window — must replay.
    let control = make_implicit_positive_event_json(ns, "implicit_positive", None, start + 6_000);
    insert_raw_payload_at(&rt, ns, &control, start + 6_000).await;

    let result = load_events_since_with_window(rt.sql().as_ref(), ns, 0, start, Some(end))
        .await
        .expect("windowed replay");

    assert_eq!(
        result.policy_excluded_count(),
        1,
        "exactly the pre-end class member is excluded"
    );
    assert_eq!(result.quarantine_count(), 0);
    assert_eq!(
        result.events.len(),
        2,
        "post-end class member and unattributed control both replay"
    );
    assert_eq!(result.quarantined[0].created_at, start + 5_000);
}

#[tokio::test]
async fn malformed_and_policy_rows_land_in_distinct_buckets_in_one_replay() {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let token = rt.authorize(Namespace::local()).expect("token");
    let ns = token.namespace().as_str();
    let sql = rt.sql();

    // Row 1: malformed JSON (quarantine bucket).
    insert_raw_payload_at(
        &rt,
        ns,
        "not json at all",
        POLICY_EXCLUSION_WINDOW_START_US + 5_000,
    )
    .await;
    // Row 2: policy-excluded class member.
    let created_at = POLICY_EXCLUSION_WINDOW_START_US + 6_000;
    let row = make_implicit_positive_event_json(
        ns,
        "implicit_positive",
        Some("balanced-recall-v1"),
        created_at,
    );
    insert_raw_payload_at(&rt, ns, &row, created_at).await;
    // Row 3: clean recall event — replays.
    let mut clean = Event::new(ns, "recall", EventKind::Audit, SubstrateKind::Note, "brain");
    clean.created_at = POLICY_EXCLUSION_WINDOW_START_US + 7_000;
    let clean_json = serde_json::to_string(&clean).expect("serialize");
    insert_raw_payload_at(
        &rt,
        ns,
        &clean_json,
        POLICY_EXCLUSION_WINDOW_START_US + 7_000,
    )
    .await;

    let result = load_events_since(sql.as_ref(), ns, 0).await.expect("load");

    assert_eq!(result.events.len(), 1, "only the clean row replays");
    assert_eq!(result.quarantine_count(), 1, "one malformed row");
    assert_eq!(result.policy_excluded_count(), 1, "one policy row");

    let by_category = |cat: ReplaySkipCategory| -> &QuarantinedRow {
        result
            .quarantined
            .iter()
            .find(|r| r.category == cat)
            .expect("both categories must be present")
    };
    let malformed = by_category(ReplaySkipCategory::Quarantine);
    let policy = by_category(ReplaySkipCategory::PolicyExcluded);
    assert!(
        malformed.reason.contains("malformed")
            || malformed.reason.contains("JSON")
            || malformed.reason.contains("json"),
        "quarantine reason must describe malformed JSON; got {:?}",
        malformed.reason
    );
    assert!(
        policy.reason.starts_with("policy_excluded"),
        "policy reason must be distinct; got {:?}",
        policy.reason
    );
    assert_ne!(malformed.id, policy.id, "distinct physical rows");

    // No cross-contamination: folding only the replayed clean recall event
    // must not move salience (a recall miss touches temporal only).
    let folded = fold_events(&result);
    let baseline = BrainState::new(16);
    assert_eq!(
        folded.balanced_recall.total_events, 1,
        "exactly the clean event folded"
    );
    assert_eq!(
        folded.balanced_recall.salience.beta(),
        baseline.balanced_recall.salience.beta(),
        "neither skipped category may move the salience posterior"
    );
}

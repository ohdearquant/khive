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

async fn insert_raw_payload(rt: &KhiveRuntime, namespace: &str, payload: &str) {
    insert_raw_payload_at(rt, namespace, payload, 1_000_000).await;
}

fn make_valid_event_json(namespace: &str) -> String {
    let ev = Event::new(
        namespace,
        "recall",
        EventKind::Audit,
        SubstrateKind::Note,
        "brain",
    );
    serde_json::to_string(&ev).expect("serialize event")
}

/// Build a brain.feedback event JSON with optional section_signals payload.
fn make_feedback_event_json(namespace: &str, section_signals: Option<serde_json::Value>) -> String {
    let mut ev = Event::new(
        namespace,
        "brain.feedback",
        EventKind::Audit,
        SubstrateKind::Event,
        "brain",
    );
    ev.target_id = Some(Uuid::new_v4());
    let mut payload = serde_json::json!({"signal": "useful"});
    if let Some(ss) = section_signals {
        payload["section_signals"] = ss;
    }
    ev.payload = payload;
    serde_json::to_string(&ev).expect("serialize feedback event")
}

// ── structural quarantine (pre-existing) ──────────────────────────────────

#[tokio::test]
async fn malformed_json_rows_are_quarantined_not_panicked() {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let token = rt.authorize(Namespace::local()).expect("token");
    let ns = token.namespace().as_str();
    let sql = rt.sql();

    // Insert one malformed row (not valid JSON) and one valid serialized Event.
    insert_raw_payload(&rt, ns, "this is not valid json {{").await;
    insert_raw_payload(&rt, ns, &make_valid_event_json(ns)).await;

    // load_events_since must return without panicking, quarantining the bad row.
    let result = load_events_since(sql.as_ref(), ns, 0)
        .await
        .expect("load must not fail on malformed rows");

    // Only the valid event should be returned.
    assert_eq!(
        result.events.len(),
        1,
        "one valid event expected; malformed row must be quarantined, not panic"
    );
    assert_eq!(result.quarantine_count(), 1, "quarantine_count must be 1");
}

#[tokio::test]
async fn all_malformed_rows_quarantined_returns_empty_vec() {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let token = rt.authorize(Namespace::local()).expect("token");
    let ns = token.namespace().as_str();
    let sql = rt.sql();

    insert_raw_payload(&rt, ns, "bad json").await;
    insert_raw_payload(&rt, ns, "{invalid}").await;

    let result = load_events_since(sql.as_ref(), ns, 0)
        .await
        .expect("load must succeed even when all rows are malformed");

    assert!(
        result.events.is_empty(),
        "all malformed rows must be quarantined"
    );
    assert_eq!(result.quarantine_count(), 2, "quarantine_count must be 2");
}

#[tokio::test]
async fn clean_rows_replay_without_quarantine() {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let token = rt.authorize(Namespace::local()).expect("token");
    let ns = token.namespace().as_str();
    let sql = rt.sql();

    for _ in 0..3 {
        insert_raw_payload(&rt, ns, &make_valid_event_json(ns)).await;
    }

    let result = load_events_since(sql.as_ref(), ns, 0)
        .await
        .expect("clean rows must replay without error");

    assert_eq!(result.events.len(), 3, "all 3 clean rows must be returned");
    assert_eq!(result.quarantine_count(), 0, "quarantine_count must be 0");
}

// ── semantic quarantine (BRAIN-007 new coverage) ──────────────────────────

#[tokio::test]
async fn empty_section_signals_quarantined() {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let token = rt.authorize(Namespace::local()).expect("token");
    let ns = token.namespace().as_str();
    let sql = rt.sql();

    // brain.feedback with section_signals: {} must be quarantined whole.
    let poison = make_feedback_event_json(ns, Some(serde_json::json!({})));
    insert_raw_payload(&rt, ns, &poison).await;

    let result = load_events_since(sql.as_ref(), ns, 0)
        .await
        .expect("load must not fail");

    assert!(
        result.events.is_empty(),
        "empty section_signals must be quarantined"
    );
    assert_eq!(result.quarantine_count(), 1);
}

#[tokio::test]
async fn unknown_section_signals_quarantined() {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let token = rt.authorize(Namespace::local()).expect("token");
    let ns = token.namespace().as_str();
    let sql = rt.sql();

    // Unknown section key must be quarantined.
    let poison = make_feedback_event_json(
        ns,
        Some(serde_json::json!({"not_a_real_section": "useful"})),
    );
    insert_raw_payload(&rt, ns, &poison).await;

    let result = load_events_since(sql.as_ref(), ns, 0)
        .await
        .expect("load must not fail");

    assert!(
        result.events.is_empty(),
        "unknown section must be quarantined"
    );
    assert_eq!(result.quarantine_count(), 1);
}

#[tokio::test]
async fn semantic_signal_in_section_signals_quarantined() {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let token = rt.authorize(Namespace::local()).expect("token");
    let ns = token.namespace().as_str();
    let sql = rt.sql();

    // Section fold only accepts useful|not_useful|wrong; semantic event kinds
    // (explicit_positive, correction, …) must be quarantined.
    let poison = make_feedback_event_json(
        ns,
        Some(serde_json::json!({"overview": "explicit_positive"})),
    );
    insert_raw_payload(&rt, ns, &poison).await;

    let result = load_events_since(sql.as_ref(), ns, 0)
        .await
        .expect("load must not fail");

    assert!(
        result.events.is_empty(),
        "semantic signal in section_signals must be quarantined"
    );
    assert_eq!(result.quarantine_count(), 1);
}

// ── state isolation: bad rows must not advance posterior state ────────────

/// Seed a snapshot, insert bad rows at FIRST / LAST / interleaved positions,
/// then call the real ensure_loaded path and assert posterior state is unchanged.
async fn seed_snapshot(rt: &KhiveRuntime, namespace: &str) -> BrainStateSnapshot {
    let state = BrainState::new(16);
    let snapshot = state.to_snapshot();
    let sql = rt.sql();
    upsert_snapshot(sql.as_ref(), namespace, &snapshot, 500_000)
        .await
        .expect("seed snapshot");
    snapshot
}

#[tokio::test]
async fn unjudged_replay_is_retained_without_training_or_quarantine() {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let mixed = rt
        .authorize(Namespace::parse("unjudged-mixed").unwrap())
        .unwrap();
    let control = rt
        .authorize(Namespace::parse("unjudged-control").unwrap())
        .unwrap();
    let sql = rt.sql();
    let baseline = seed_snapshot(&rt, control.namespace().as_str()).await;
    upsert_snapshot(sql.as_ref(), mixed.namespace().as_str(), &baseline, 500_000)
        .await
        .unwrap();
    let baseline_json = serde_json::to_value(&baseline).unwrap();

    let mut feedback = Event::new(
        control.namespace().as_str(),
        "brain.feedback",
        EventKind::FeedbackExplicit,
        SubstrateKind::Event,
        "brain",
    );
    feedback.target_id = Some(Uuid::new_v4());
    feedback.payload = serde_json::json!({"signal": "useful",
            "section_signals": {"overview": "useful"},
            "served_by_profile_id": "balanced-recall-v1"});
    let mut telemetry = Vec::new();
    for (verb, signal, sections) in [
        ("brain.feedback", "unjudged", serde_json::Value::Null),
        (
            "brain.feedback",
            "useful",
            serde_json::json!({"overview": "useful"}),
        ),
        (
            "brain.feedback",
            "implicit_positive",
            serde_json::json!({"unknown": "wrong"}),
        ),
        ("brain.section_feedback", "useful", serde_json::json!({})),
    ] {
        let mut event = feedback.clone();
        event.id = Uuid::new_v4();
        event.namespace = mixed.namespace().as_str().to_owned();
        event.verb = verb.to_owned();
        event.kind = EventKind::FeedbackUnjudged;
        event.created_at = POLICY_EXCLUSION_WINDOW_START_US + 1;
        event.payload = serde_json::json!({"signal": signal, "section_signals": sections,
                "served_by_profile_id": "telemetry-only-profile"});
        append_brain_event(
            sql.as_ref(),
            &event.namespace,
            "telemetry-only-profile",
            &event.verb,
            &serde_json::to_value(&event).unwrap(),
            600_001 + telemetry.len() as i64,
        )
        .await
        .unwrap();
        telemetry.push(event);
    }
    let loaded = load_events_since(sql.as_ref(), mixed.namespace().as_str(), 500_000)
        .await
        .unwrap();
    assert_eq!(loaded.events, telemetry);
    assert!(
        loaded.quarantined.is_empty(),
        "telemetry must not be quarantined or policy-excluded"
    );
    let state = Mutex::new(BrainState::new(16));
    ensure_loaded(
        &rt,
        &mixed,
        &Mutex::new(PersistenceTracker::new()),
        &state,
        16,
    )
    .await
    .unwrap();
    assert_eq!(
        serde_json::to_value(state.lock().unwrap().to_snapshot()).unwrap(),
        baseline_json
    );

    let mut snapshots = Vec::new();
    for token in [&control, &mixed] {
        feedback.namespace = token.namespace().as_str().to_owned();
        append_brain_event(
            sql.as_ref(),
            &feedback.namespace,
            "balanced-recall-v1",
            &feedback.verb,
            &serde_json::to_value(&feedback).unwrap(),
            700_001,
        )
        .await
        .unwrap();
        let mut malformed = feedback.clone();
        malformed.id = Uuid::new_v4();
        malformed.payload["section_signals"] = serde_json::json!({"unknown": "wrong"});
        append_brain_event(
            sql.as_ref(),
            &malformed.namespace,
            "balanced-recall-v1",
            &malformed.verb,
            &serde_json::to_value(&malformed).unwrap(),
            700_002,
        )
        .await
        .unwrap();
        let loaded = load_events_since(sql.as_ref(), token.namespace().as_str(), 500_000)
            .await
            .unwrap();
        assert_eq!(loaded.quarantine_count(), 1);
        assert_eq!(loaded.policy_excluded_count(), 0);
        assert!(loaded.quarantined[0].reason.contains("section_signals"));
        assert!(loaded.events.contains(&feedback));

        let state = Mutex::new(BrainState::new(16));
        ensure_loaded(
            &rt,
            token,
            &Mutex::new(PersistenceTracker::new()),
            &state,
            16,
        )
        .await
        .unwrap();
        let state = state.lock().unwrap();
        assert_eq!(state.balanced_recall.total_events, 1);
        assert_eq!(
            state
                .balanced_recall
                .entity_posteriors
                .get(&feedback.target_id.unwrap())
                .unwrap()
                .alpha(),
            2.0
        );
        assert_eq!(state.section_states["balanced-recall-v1"].total_events, 1);
        assert!(!state.section_states.contains_key("telemetry-only-profile"));
        assert!(!state.profiles.contains_key("telemetry-only-profile"));
        assert!(!state.profile_states.contains_key("telemetry-only-profile"));
        snapshots.push(serde_json::to_value(state.to_snapshot()).unwrap());
    }
    assert_ne!(
        snapshots[0], baseline_json,
        "ordinary feedback must still train"
    );
    assert_eq!(
        snapshots[0], snapshots[1],
        "unjudged rows must not affect any persisted brain state"
    );
}

/// Assert that section posteriors and epoch are at the initial (baseline) values,
/// meaning no section-fold mutation occurred from any replayed event.
/// Does NOT assert balanced_recall state — clean recall/search events legitimately
/// advance that without touching section state.
fn assert_section_posteriors_at_baseline(state: &BrainState, baseline: &BrainState) {
    for key in state.section_states.keys() {
        let s = &state.section_states[key];
        let b = &baseline.section_states[key];
        assert_eq!(
            s.total_events, b.total_events,
            "section_states[{key}].total_events changed; bad row must not advance section state"
        );
        assert_eq!(
                s.exploration_epoch, b.exploration_epoch,
                "section_states[{key}].exploration_epoch changed; bad row must not advance section state"
            );
        for (st, p) in &s.posteriors {
            let bp = &b.posteriors[st];
            assert!(
                    (p.alpha() - bp.alpha()).abs() < 1e-12
                        && (p.beta() - bp.beta()).abs() < 1e-12,
                    "section posterior for {:?} changed: got alpha={} beta={}, expected alpha={} beta={}; \
                     bad row must not mutate posteriors",
                    st, p.alpha(), p.beta(), bp.alpha(), bp.beta()
                );
        }
    }
}

#[tokio::test]
async fn bad_row_first_does_not_mutate_posterior_state() {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let token = rt.authorize(Namespace::local()).expect("token");
    let ns = token.namespace().as_str();

    seed_snapshot(&rt, ns).await;

    // Row 1 (bad — semantic poison): created_at=600_000 (after snapshot at 500_000)
    let poison = make_feedback_event_json(ns, Some(serde_json::json!({"overview": "correction"})));
    insert_raw_payload_at(&rt, ns, &poison, 600_001).await;

    // Row 2 (clean recall event): created_at=600_002
    insert_raw_payload_at(&rt, ns, &make_valid_event_json(ns), 600_002).await;

    // Use load_events_since directly and replay manually to assert isolation.
    let sql = rt.sql();
    let result = load_events_since(sql.as_ref(), ns, 500_000)
        .await
        .expect("load must not fail");

    assert_eq!(
        result.quarantine_count(),
        1,
        "bad first row must be quarantined"
    );
    assert_eq!(result.events.len(), 1, "one clean event must pass through");

    // Apply the clean events to a fresh state and confirm section state is at baseline.
    let baseline = BrainState::new(16);
    let mut state = BrainState::new(16);
    for event in &result.events {
        let signal = crate::event::interpret(event);
        state.balanced_recall.apply_signal(&signal);
        for section_state in state.section_states.values_mut() {
            section_state.apply_signal(&signal);
        }
    }
    // The single clean event is a recall, not a feedback; section state unchanged.
    assert_section_posteriors_at_baseline(&state, &baseline);
}

#[tokio::test]
async fn bad_row_last_does_not_mutate_posterior_state() {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let token = rt.authorize(Namespace::local()).expect("token");
    let ns = token.namespace().as_str();

    seed_snapshot(&rt, ns).await;

    // Row 1 (clean): created_at=600_001
    insert_raw_payload_at(&rt, ns, &make_valid_event_json(ns), 600_001).await;
    // Row 2 (bad — empty section_signals): created_at=600_002
    let poison = make_feedback_event_json(ns, Some(serde_json::json!({})));
    insert_raw_payload_at(&rt, ns, &poison, 600_002).await;

    let sql = rt.sql();
    let result = load_events_since(sql.as_ref(), ns, 500_000)
        .await
        .expect("load must not fail");

    assert_eq!(
        result.quarantine_count(),
        1,
        "bad last row must be quarantined"
    );
    assert_eq!(result.events.len(), 1, "one clean event must pass through");

    let baseline = BrainState::new(16);
    let mut state = BrainState::new(16);
    for event in &result.events {
        let signal = crate::event::interpret(event);
        state.balanced_recall.apply_signal(&signal);
        for section_state in state.section_states.values_mut() {
            section_state.apply_signal(&signal);
        }
    }
    assert_section_posteriors_at_baseline(&state, &baseline);
}

#[tokio::test]
async fn bad_rows_interleaved_do_not_mutate_posterior_state() {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let token = rt.authorize(Namespace::local()).expect("token");
    let ns = token.namespace().as_str();

    seed_snapshot(&rt, ns).await;

    // clean, bad, clean, bad, clean
    insert_raw_payload_at(&rt, ns, &make_valid_event_json(ns), 600_001).await;
    let p1 = make_feedback_event_json(
        ns,
        Some(serde_json::json!({"overview": "explicit_negative"})),
    );
    insert_raw_payload_at(&rt, ns, &p1, 600_002).await;
    insert_raw_payload_at(&rt, ns, &make_valid_event_json(ns), 600_003).await;
    let p2 = make_feedback_event_json(ns, Some(serde_json::json!({})));
    insert_raw_payload_at(&rt, ns, &p2, 600_004).await;
    insert_raw_payload_at(&rt, ns, &make_valid_event_json(ns), 600_005).await;

    let sql = rt.sql();
    let result = load_events_since(sql.as_ref(), ns, 500_000)
        .await
        .expect("load must not fail");

    assert_eq!(
        result.quarantine_count(),
        2,
        "2 bad rows must be quarantined"
    );
    assert_eq!(result.events.len(), 3, "3 clean rows must pass through");

    // Apply only clean events; section posteriors must stay at baseline.
    let baseline = BrainState::new(16);
    let mut state = BrainState::new(16);
    for event in &result.events {
        let signal = crate::event::interpret(event);
        state.balanced_recall.apply_signal(&signal);
        for section_state in state.section_states.values_mut() {
            section_state.apply_signal(&signal);
        }
    }
    assert_section_posteriors_at_baseline(&state, &baseline);
}

// ── quarantine metadata: id and reason must be returned, not just counted ──

#[tokio::test]
async fn quarantined_rows_return_id_and_reason() {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let token = rt.authorize(Namespace::local()).expect("token");
    let ns = token.namespace().as_str();
    let sql = rt.sql();

    // Two bad rows: one malformed JSON, one semantic poison.
    insert_raw_payload(&rt, ns, "not json at all").await;
    let poison = make_feedback_event_json(ns, Some(serde_json::json!({})));
    insert_raw_payload(&rt, ns, &poison).await;

    let result = load_events_since(sql.as_ref(), ns, 0)
        .await
        .expect("load must not fail");

    assert_eq!(
        result.quarantine_count(),
        2,
        "both bad rows must be quarantined"
    );
    assert!(result.events.is_empty(), "no clean events expected");

    // Each quarantined entry must carry a non-zero id, non-empty reason,
    // correct profile_id, non-zero created_at, and non-empty payload_snippet.
    for qr in &result.quarantined {
        assert!(
            qr.id > 0,
            "quarantined row id must be a real autoincrement id; got {}",
            qr.id
        );
        assert!(
            !qr.reason.is_empty(),
            "quarantined row must carry a non-empty reason string"
        );
        assert_eq!(
            qr.profile_id, "test-profile",
            "quarantined row profile_id must match inserted value; got {:?}",
            qr.profile_id
        );
        assert!(
            qr.created_at > 0,
            "quarantined row created_at must be non-zero; got {}",
            qr.created_at
        );
        assert!(
            !qr.payload_snippet.is_empty(),
            "quarantined row payload_snippet must be non-empty"
        );
    }

    // First row: malformed JSON — reason must mention JSON or malformed.
    assert!(
        result.quarantined[0].reason.contains("malformed")
            || result.quarantined[0].reason.contains("JSON")
            || result.quarantined[0].reason.contains("json"),
        "first quarantine reason must describe malformed JSON; got: {:?}",
        result.quarantined[0].reason
    );
    // First row payload_snippet must contain a recognizable fragment of the bad payload.
    assert!(
        result.quarantined[0].payload_snippet.contains("not json"),
        "first row snippet must contain 'not json'; got: {:?}",
        result.quarantined[0].payload_snippet
    );

    // Second row: semantic poison (empty section_signals) — reason must mention section_signals.
    assert!(
        result.quarantined[1].reason.contains("section_signals"),
        "second quarantine reason must mention section_signals; got: {:?}",
        result.quarantined[1].reason
    );
    // Second row payload_snippet must be non-empty (it's valid JSON, so it has content).
    assert!(
        !result.quarantined[1].payload_snippet.is_empty(),
        "second row payload_snippet must be non-empty; got: {:?}",
        result.quarantined[1].payload_snippet
    );
}

// ── char-boundary safety: multibyte payload does not panic ───────────────

#[tokio::test]
async fn payload_snippet_truncation_safe_on_multibyte_chars() {
    let rt = KhiveRuntime::memory().expect("memory runtime");
    let token = rt.authorize(Namespace::local()).expect("token");
    let ns = token.namespace().as_str();
    let sql = rt.sql();

    // '日' is 3 bytes in UTF-8; 250 repetitions = 750 bytes, well over the 200-byte limit.
    // A naive &s[..200] would land mid-char and panic.
    let long_multibyte: String = "日".repeat(250);
    insert_raw_payload(&rt, ns, &long_multibyte).await;

    // Must not panic.
    let result = load_events_since(sql.as_ref(), ns, 0)
        .await
        .expect("load must not fail");

    assert_eq!(result.quarantine_count(), 1);
    let qr = &result.quarantined[0];

    // Snippet must be valid UTF-8 (Rust strings are always valid UTF-8, but
    // verify it is non-empty and ≤ 200 bytes as a byte-length check).
    assert!(
        !qr.payload_snippet.is_empty(),
        "snippet must be non-empty for a non-empty payload"
    );
    // The trailing ellipsis is a multi-byte char itself; strip it before measuring.
    let snippet_body = qr.payload_snippet.trim_end_matches('…');
    assert!(
        snippet_body.len() <= 200,
        "snippet body (sans ellipsis) must be ≤200 bytes; got {} bytes",
        snippet_body.len()
    );
    // The snippet body must itself be valid UTF-8 (Rust ensures this, but also
    // assert it only contains the expected character).
    assert!(
        snippet_body.chars().all(|c| c == '日'),
        "snippet body must contain only '日' chars; got: {:?}",
        snippet_body
    );
}

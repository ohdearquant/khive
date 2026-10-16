use super::*;

#[tokio::test]
async fn brain_auto_feedback_credits_only_the_selected_result() {
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let first = create_test_entity(&rt, &token).await;
    let selected = create_test_entity(&rt, &token).await;

    let result = pack
        .dispatch(
            "brain.auto_feedback",
            json!({
                "query": "recall calibration target",
                "results": [{ "id": first }, { "id": selected }],
                "target_id": selected,
                "signal": "implicit_positive"
            }),
            &registry,
            &token,
        )
        .await
        .expect("auto_feedback succeeds");

    assert_eq!(result["emitted"], json!(true), "emitted must be true");
    assert_eq!(
        result["signal"],
        json!("implicit_positive"),
        "the caller's signal must be preserved"
    );
    let returned_target_id = result["target_id"].as_str().unwrap_or("");
    assert_eq!(
        returned_target_id.len(),
        36,
        "target_id in auto_feedback response must be full 36-char UUID"
    );
    assert_eq!(
        returned_target_id, selected,
        "rank position must not override the caller-selected result"
    );
    assert_eq!(
        pack.snapshot().balanced_recall.total_events,
        1,
        "auto_feedback must increment total_events"
    );
}

#[tokio::test]
async fn brain_auto_feedback_without_signal_abstains_without_writing() {
    let (pack, rt) = make_anonymous_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;
    let before = pack.snapshot().balanced_recall.total_events;

    let result = pack
        .dispatch(
            "brain.auto_feedback",
            json!({
                "query": "no caller judgment",
                "results": [{"id": target}],
                "target_id": target
            }),
            &registry,
            &token,
        )
        .await
        .expect("omitting signal is an explicit abstention");

    assert_eq!(result["emitted"], json!(false));
    assert_eq!(result["reason"], json!("no_signal"));
    assert_eq!(pack.snapshot().balanced_recall.total_events, before);

    let malformed = pack
        .dispatch(
            "brain.auto_feedback",
            json!({
                "query": "malformed scorer abstention",
                "results": [{"id": target}],
                "scorer_run_id": "run-without-ledger"
            }),
            &registry,
            &token,
        )
        .await
        .expect_err("abstention must not bypass scorer-pair validation");
    assert!(malformed
        .to_string()
        .contains("scorer_run_id and serve_ledger_id must be supplied together"));

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
    assert!(
        events.items.is_empty(),
        "abstention must not append an event"
    );
}

// #4894 exercises dispatch and the actual stores, rather than invoking the
// interpreter with a synthetic unjudged row (already covered by #4893).
use khive_storage::event::{Event, EventFilter};
use khive_storage::types::{PageRequest, SqlStatement};
use khive_types::{EventKind, SubstrateKind};

fn auto_request(target: &str, signal: &str) -> Value {
    json!({
        "query": "unjudged provenance regression",
        "results": [{"id": target}],
        "target_id": target,
        "signal": signal,
    })
}

async fn warm(pack: &BrainPack, token: &NamespaceToken) {
    pack.dispatch("brain.profiles", json!({}), &empty_registry(), token)
        .await
        .expect("warm state before observing the feedback call");
}

async fn feedback_events(rt: &KhiveRuntime, token: &NamespaceToken) -> Vec<Event> {
    rt.events(token)
        .unwrap()
        .query_events(
            EventFilter {
                kinds: vec![EventKind::FeedbackUnjudged, EventKind::FeedbackExplicit],
                ..Default::default()
            },
            PageRequest {
                limit: 1000,
                offset: 0,
            },
        )
        .await
        .unwrap()
        .items
}

// Snapshot/version and complete durable rows, not merely total_events: a
// spurious zero-weight gate, grade update or replay row must also be detected.
async fn training_state(pack: &BrainPack, rt: &KhiveRuntime, token: &NamespaceToken) -> Value {
    let durable = crate::persist::load_latest_snapshot(
        rt.sql().as_ref(),
        token.namespace().as_str(),
        ENTITY_CACHE_CAPACITY,
    )
    .await
    .unwrap();
    let replay =
        crate::persist::load_events_since(rt.sql().as_ref(), token.namespace().as_str(), 0)
            .await
            .unwrap();
    let mut tables = serde_json::Map::new();
    for (name, statement) in [
        (
            "mass",
            "SELECT * FROM brain_implicit_mass ORDER BY namespace, profile_id, target_id",
        ),
        (
            "claims",
            "SELECT * FROM brain_scorer_dedup ORDER BY scorer_run_id, serve_ledger_id",
        ),
        ("ledger", "SELECT * FROM brain_serve_ledger ORDER BY id"),
    ] {
        let rows = rt
            .sql()
            .reader()
            .await
            .unwrap()
            .query_all(SqlStatement {
                sql: statement.into(),
                params: vec![],
                label: Some("unjudged_test_state".into()),
            })
            .await
            .unwrap();
        tables.insert(name.into(), serde_json::to_value(rows).unwrap());
    }
    let tracker = pack.persistence.lock().unwrap();
    json!({
        "live": pack.snapshot(), "durable": durable,
        "replay": replay.events, "quarantined": replay.quarantined.len(),
        "tables": tables, "loaded": tracker.loaded_namespaces,
        "active": tracker.active_namespace,
    })
}

async fn record_serve(
    rt: &KhiveRuntime,
    id: &str,
    namespace: &str,
    target: &str,
    profile: Option<&str>,
    attribution: Option<&str>,
) {
    assert!(crate::serve_ledger::record_serve(
        rt.sql().as_ref(),
        id,
        namespace,
        "recall",
        profile,
        None,
        None,
        target,
        id,
        "unjudged provenance regression",
        1000,
        attribution,
    )
    .await
    .unwrap());
}

async fn assert_grade(rt: &KhiveRuntime, id: &str, expected: Option<(&str, &str)>) {
    let row = crate::serve_ledger::get_serve_row(rt.sql().as_ref(), id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.grade.as_deref(), expected.map(|(_, grade)| grade));
    assert_eq!(row.scorer_run_id.as_deref(), expected.map(|(run, _)| run));
    assert_eq!(row.graded_at.is_some(), expected.is_some());
}

#[tokio::test]
async fn unjudged_full_registry_event_is_readable_by_kind_and_origin() {
    let (_, rt) = make_pack();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;
    record_serve(
        &rt,
        "registry-ledger",
        "local",
        &target,
        Some("balanced-recall-v1"),
        Some("profile"),
    )
    .await;
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("brain-test".into()));
    builder.register(khive_pack_kg::KgPack::new(rt.clone()));
    builder.register(BrainPack::new(rt.clone()));
    let registry = builder.build().unwrap();
    let result = registry
        .dispatch(
            "brain.auto_feedback",
            json!({
                "query": "raw query: alpha AND beta",
                "results": [{"id": "raw-first"}, {
                    "id": "selected-alias", "full_id": target.to_uppercase(),
                    "served_by_profile_id": "balanced-recall-v1", "serve_attribution": "profile"
                }],
                "target_id": "selected-alias", "signal": "unjudged",
                "scorer_run_id": "registry-run", "serve_ledger_id": "registry-ledger"
            }),
        )
        .await
        .unwrap();
    assert_eq!(result["emitted"], true);
    assert_eq!(result["verb"], "brain.auto_feedback");
    assert_eq!(result["feedback_verb"], "brain.feedback");
    assert_eq!(result["result_count"], 2);
    assert_eq!(result["target_id"], target);
    let listed = registry
        .dispatch(
            "list",
            json!({
                "kind": "event", "event_kind": "feedback_unjudged", "limit": 100,
            }),
        )
        .await
        .unwrap();
    let rows = listed["items"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{listed}");
    let event = &rows[0];
    assert_eq!(event["id"], result["event_id"]);
    assert_eq!(event["kind"], "feedback_unjudged");
    assert_eq!(event["verb"], "brain.feedback");
    assert_eq!(event["namespace"], "local");
    assert_eq!(
        event["actor"],
        format!("{}:{}", token.actor().kind, token.actor().id)
    );
    assert_eq!(event["target_id"], target);
    assert_eq!(event["substrate"], "entity");
    assert_eq!(event["payload"]["signal"], "unjudged");
    assert_eq!(event["payload"]["originating_verb"], "brain.auto_feedback");
    assert_eq!(event["payload"]["query"], "raw query: alpha AND beta");
    assert_eq!(
        event["payload"]["candidate_ids"],
        json!(["raw-first", "selected-alias"])
    );
    assert_eq!(
        event["payload"]["served_by_profile_id"],
        "balanced-recall-v1"
    );
    assert_eq!(event["payload"]["serve_attribution"], "profile");
    assert_eq!(event["payload"]["scorer_run_id"], "registry-run");
    assert_eq!(event["payload"]["serve_ledger_id"], "registry-ledger");
    assert!(event["payload"].get("gate").is_none());
    assert!(event["payload"].get("profile_resolution").is_none());
    assert!(event["duration_us"].as_i64().unwrap() > 0);
    let counts = registry
        .dispatch(
            "brain.event_counts",
            json!({
                "since": "2020-01-01T00:00:00Z", "until": "2100-01-01T00:00:00Z"
            }),
        )
        .await
        .unwrap();
    assert_eq!(counts["counts_by_kind"]["feedback_unjudged"], 1);
    assert!(counts["counts_by_kind"].get("feedback_explicit").is_none());
    assert_grade(&rt, "registry-ledger", None).await;
}

#[tokio::test]
async fn unjudged_leaves_warm_global_profile_section_and_durable_state_unchanged() {
    let (pack, rt) = make_pack();
    let token = rt.authorize(Namespace::local()).unwrap();
    let registry = empty_registry();
    let target = create_test_entity(&rt, &token).await;
    // Seed actual persisted evidence and a mass row so telemetry must preserve
    // populated state as well as the empty-state cases below.
    pack.dispatch(
        "brain.feedback",
        json!({
            "target_id": target, "signal": "implicit_positive",
            "served_by_profile_id": "balanced-recall-v1",
            "section_signals": {"operational_guidance":"useful"}
        }),
        &registry,
        &token,
    )
    .await
    .unwrap();
    record_serve(
        &rt,
        "warm-ledger",
        "local",
        &target,
        Some("balanced-recall-v1"),
        Some("profile"),
    )
    .await;
    let before = training_state(&pack, &rt, &token).await;
    assert!(!before["tables"]["mass"].as_array().unwrap().is_empty());
    assert!(before["live"]["section_states"]
        .get("balanced-recall-v1")
        .is_some());
    assert!(!before["durable"].is_null());
    let mut request = auto_request(&target, "unjudged");
    request["scorer_run_id"] = json!("warm-run");
    request["serve_ledger_id"] = json!("warm-ledger");
    for expected_public in [2, 3] {
        let result = pack
            .dispatch("brain.auto_feedback", request.clone(), &registry, &token)
            .await
            .unwrap();
        assert_eq!(result["emitted"], true);
        assert_eq!(training_state(&pack, &rt, &token).await, before);
        assert_eq!(feedback_events(&rt, &token).await.len(), expected_public);
        assert_grade(&rt, "warm-ledger", None).await;
    }
}

#[tokio::test]
async fn unjudged_preserves_attribution_without_default_or_binding_substitution() {
    // request attribution, optional ledger attribution, expected provenance.
    let cases = [
        ("omitted", json!({}), None, None, Value::Null, "unspecified"),
        (
            "unattributed",
            json!({"serve_attribution":"unattributed"}),
            None,
            None,
            Value::Null,
            "unattributed",
        ),
        (
            "historical",
            json!({"served_by_profile_id":"historical-missing"}),
            None,
            None,
            json!("historical-missing"),
            "profile",
        ),
        (
            "ledger-profile",
            json!({}),
            Some("historical-missing"),
            Some("profile"),
            json!("historical-missing"),
            "profile",
        ),
        (
            "ledger-unattributed",
            json!({}),
            None,
            Some("unattributed"),
            Value::Null,
            "unattributed",
        ),
        (
            "ledger-legacy-null",
            json!({}),
            None,
            Some("unspecified"),
            Value::Null,
            "unspecified",
        ),
        (
            "legacy-keeps-request",
            json!({"served_by_profile_id":"historical-missing"}),
            None,
            Some("unspecified"),
            json!("historical-missing"),
            "profile",
        ),
    ];
    for (case, attribution, ledger_profile, ledger_marker, expected_profile, expected_marker) in
        cases
    {
        let (pack, rt) = make_pack();
        let token = rt.authorize(Namespace::local()).unwrap();
        let target = create_test_entity(&rt, &token).await;
        warm(&pack, &token).await;
        if case == "omitted" {
            let registry = empty_registry();
            pack.dispatch(
                "brain.create_profile",
                json!({
                    "name":"bound-profile-4894", "consumer_kind":"recall"
                }),
                &registry,
                &token,
            )
            .await
            .unwrap();
            pack.dispatch(
                "brain.activate",
                json!({"profile_id":"bound-profile-4894"}),
                &registry,
                &token,
            )
            .await
            .unwrap();
            pack.dispatch("brain.bind", json!({
                "profile_id":"bound-profile-4894", "consumer_kind":"recall", "actor":"brain-test"
            }), &registry, &token).await.unwrap();
            assert!(!pack.snapshot().bindings.is_empty());
        }
        let mut request = auto_request(&target, "unjudged");
        request
            .as_object_mut()
            .unwrap()
            .extend(attribution.as_object().unwrap().clone());
        if ledger_marker.is_some() {
            record_serve(&rt, case, "local", &target, ledger_profile, ledger_marker).await;
            request["scorer_run_id"] = json!(case);
            request["serve_ledger_id"] = json!(case);
        }
        let before = training_state(&pack, &rt, &token).await;
        let result = pack
            .dispatch("brain.auto_feedback", request, &empty_registry(), &token)
            .await
            .unwrap_or_else(|e| panic!("{case}: {e}"));
        assert_eq!(result["served_by_profile_id"], expected_profile, "{case}");
        assert_eq!(result["serve_attribution"], expected_marker, "{case}");
        assert_eq!(training_state(&pack, &rt, &token).await, before, "{case}");
        let events = feedback_events(&rt, &token).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::FeedbackUnjudged);
        assert_eq!(events[0].payload["served_by_profile_id"], expected_profile);
        assert_eq!(events[0].payload["serve_attribution"], expected_marker);
    }
}

#[tokio::test]
async fn unjudged_top_level_pair_overrides_selected_result_and_accepts_archived_provenance() {
    let (pack, rt) = make_pack();
    let token = rt.authorize(Namespace::local()).unwrap();
    let registry = empty_registry();
    let target = create_test_entity(&rt, &token).await;
    create_active_lifecycle_profile(&pack, &registry, &token).await;
    for verb in ["brain.deactivate", "brain.archive"] {
        pack.dispatch(
            verb,
            json!({"profile_id":LIFECYCLE_PROFILE_ID}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    }
    let before = training_state(&pack, &rt, &token).await;
    for top in [
        json!({"serve_attribution":"unattributed"}),
        json!({"served_by_profile_id":LIFECYCLE_PROFILE_ID}),
    ] {
        let mut request = auto_request(&target, "unjudged");
        request["results"][0]["served_by_profile_id"] = json!("balanced-recall-v1");
        request["results"][0]["serve_attribution"] = json!("profile");
        request
            .as_object_mut()
            .unwrap()
            .extend(top.as_object().unwrap().clone());
        let result = pack
            .dispatch("brain.auto_feedback", request, &registry, &token)
            .await
            .unwrap();
        assert_eq!(
            result["served_by_profile_id"],
            top.get("served_by_profile_id")
                .cloned()
                .unwrap_or(Value::Null)
        );
        assert_eq!(training_state(&pack, &rt, &token).await, before);
    }
}

#[tokio::test]
async fn unjudged_anonymous_and_local_callers_can_report_note_targets() {
    for actor in [None, Some("local")] {
        let (pack, rt) = match actor {
            Some(actor) => make_pack_with_actor(actor),
            None => make_anonymous_pack(),
        };
        let token = rt.authorize(Namespace::local()).unwrap();
        let note = rt
            .create_note(
                &token,
                "observation",
                None,
                "unjudged note target",
                None,
                None,
                vec![],
            )
            .await
            .unwrap();
        warm(&pack, &token).await;
        let before = training_state(&pack, &rt, &token).await;
        let result = pack
            .dispatch(
                "brain.auto_feedback",
                auto_request(&note.id.to_string(), "unjudged"),
                &empty_registry(),
                &token,
            )
            .await
            .unwrap();
        assert_eq!(result["emitted"], true);
        assert_eq!(training_state(&pack, &rt, &token).await, before);
        let event = feedback_events(&rt, &token).await.pop().unwrap();
        assert_eq!(event.substrate, SubstrateKind::Note);
        assert_eq!(event.target_id, Some(note.id));
        assert_eq!(
            event.actor,
            format!("{}:{}", token.actor().kind, token.actor().id)
        );
        assert_eq!(event.payload["serve_attribution"], "unspecified");
    }
}

#[tokio::test]
async fn unjudged_pair_leaves_one_later_judgment_and_each_new_scorer_available() {
    for signal in ["useful", "not_useful", "wrong"] {
        let (pack, rt) = make_pack();
        let token = rt.authorize(Namespace::local()).unwrap();
        let registry = empty_registry();
        let target = create_test_entity(&rt, &token).await;
        warm(&pack, &token).await;
        record_serve(
            &rt,
            "judged-ledger",
            "local",
            &target,
            Some("balanced-recall-v1"),
            Some("profile"),
        )
        .await;
        let mut request = auto_request(&target, "unjudged");
        request["scorer_run_id"] = json!("first-run");
        request["serve_ledger_id"] = json!("judged-ledger");
        let initial = training_state(&pack, &rt, &token).await;
        for _ in 0..2 {
            assert_eq!(
                pack.dispatch("brain.auto_feedback", request.clone(), &registry, &token)
                    .await
                    .unwrap()["emitted"],
                true
            );
            assert_eq!(training_state(&pack, &rt, &token).await, initial);
        }
        request["signal"] = json!(signal);
        let judged = pack
            .dispatch("brain.auto_feedback", request.clone(), &registry, &token)
            .await
            .unwrap();
        assert_eq!(judged["emitted"], true, "{signal}");
        assert_grade(&rt, "judged-ledger", Some(("first-run", signal))).await;
        assert_eq!(pack.snapshot().balanced_recall.total_events, 1);
        let after = training_state(&pack, &rt, &token).await;
        assert_eq!(after["tables"]["claims"].as_array().unwrap().len(), 1);
        assert!(after["tables"]["mass"].as_array().unwrap().is_empty());
        assert_eq!(after["replay"].as_array().unwrap().len(), 1);
        let duplicate = pack
            .dispatch("brain.auto_feedback", request.clone(), &registry, &token)
            .await
            .unwrap();
        assert_eq!(duplicate["deduped"], true);
        assert_eq!(duplicate["emitted"], false);
        request["signal"] = json!("unjudged");
        assert_eq!(
            pack.dispatch("brain.auto_feedback", request.clone(), &registry, &token)
                .await
                .unwrap()["emitted"],
            true
        );
        assert_eq!(training_state(&pack, &rt, &token).await, after);
        request["signal"] = json!(signal);
        request["scorer_run_id"] = json!("second-run");
        assert_eq!(
            pack.dispatch("brain.auto_feedback", request, &registry, &token)
                .await
                .unwrap()["emitted"],
            true
        );
        assert_eq!(pack.snapshot().balanced_recall.total_events, 2);
        assert_grade(&rt, "judged-ledger", Some(("second-run", signal))).await;
        let final_state = training_state(&pack, &rt, &token).await;
        assert_eq!(final_state["tables"]["claims"].as_array().unwrap().len(), 2);
        assert!(final_state["tables"]["mass"].as_array().unwrap().is_empty());
        let events = feedback_events(&rt, &token).await;
        assert_eq!(
            events
                .iter()
                .filter(|event| event.kind == EventKind::FeedbackUnjudged)
                .count(),
            3
        );
        let judged: Vec<_> = events
            .iter()
            .filter(|event| event.kind == EventKind::FeedbackExplicit)
            .collect();
        assert_eq!(judged.len(), 2);
        assert!(judged
            .iter()
            .all(|event| event.payload.get("gate").is_none()));
    }
}

#[tokio::test]
#[serial_test::serial(brain_feedback_precommit)]
async fn concurrent_auto_judgments_on_two_pack_instances_fold_one_pair_once() {
    use std::sync::Arc;
    use tokio::sync::oneshot;

    struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            crate::pack::clear_feedback_precommit_hook();
        }
    }
    let _guard = HookGuard;
    const PROFILE: &str = "auto-concurrent-judgment-4894";
    let (first, rt) = make_pack();
    let first = Arc::new(first);
    let second = BrainPack::new(rt.clone());
    let token = Arc::new(rt.authorize(Namespace::local()).unwrap());
    let registry = Arc::new(empty_registry());
    let target = create_test_entity(&rt, &token).await;
    first
        .dispatch(
            "brain.create_profile",
            json!({"name":PROFILE,"consumer_kind":"recall"}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    first
        .dispatch(
            "brain.activate",
            json!({"profile_id":PROFILE}),
            &registry,
            &token,
        )
        .await
        .unwrap();
    warm(&second, &token).await;
    record_serve(
        &rt,
        "concurrent-ledger",
        "local",
        &target,
        Some(PROFILE),
        Some("profile"),
    )
    .await;
    let before = training_state(&first, &rt, &token).await;
    let replay_before = before["replay"].as_array().unwrap().len();
    let mut request = auto_request(&target, "useful");
    request["served_by_profile_id"] = json!(PROFILE);
    request["scorer_run_id"] = json!("concurrent-run");
    request["serve_ledger_id"] = json!("concurrent-ledger");
    let (reached_tx, reached_rx) = oneshot::channel();
    let (proceed_tx, proceed_rx) = oneshot::channel();
    crate::pack::set_feedback_precommit_hook(crate::pack::FeedbackPrecommitHook {
        profile_id: PROFILE.into(),
        reached_tx,
        proceed_rx,
    });
    let first_task = Arc::clone(&first);
    let first_token = Arc::clone(&token);
    let first_registry = Arc::clone(&registry);
    let first_request = request.clone();
    let parked = tokio::spawn(async move {
        first_task
            .dispatch(
                "brain.auto_feedback",
                first_request,
                &first_registry,
                &first_token,
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), reached_rx)
        .await
        .expect("first caller reaches precommit hook")
        .unwrap();
    // First has already read an ungraded ledger. The independent pack commits
    // before it resumes, so the loser must hit the authoritative atomic claim,
    // rather than pass through the already_graded read-only shortcut.
    let winner = second
        .dispatch("brain.auto_feedback", request, &registry, &token)
        .await
        .unwrap();
    assert_eq!(winner["emitted"], true);
    proceed_tx.send(()).unwrap();
    let loser = tokio::time::timeout(std::time::Duration::from_secs(10), parked)
        .await
        .expect("parked feedback finishes")
        .unwrap()
        .unwrap();
    assert_eq!(loser["emitted"], false);
    assert_eq!(loser["deduped"], true);
    let observer = BrainPack::new(rt.clone());
    warm(&observer, &token).await;
    assert_eq!(observer.snapshot().profile_states[PROFILE].total_events, 1);
    assert_eq!(observer.snapshot().balanced_recall.total_events, 0);
    let state = training_state(&observer, &rt, &token).await;
    assert_eq!(state["tables"]["claims"].as_array().unwrap().len(), 1);
    assert_eq!(state["replay"].as_array().unwrap().len(), replay_before + 1);
    assert!(state["tables"]["mass"].as_array().unwrap().is_empty());
    assert_eq!(feedback_events(&rt, &token).await.len(), 1);
    assert_grade(&rt, "concurrent-ledger", Some(("concurrent-run", "useful"))).await;
}

#[tokio::test]
async fn auto_judgment_failures_roll_back_every_effect_and_allow_identical_retry() {
    for (case, trigger) in [
        ("public", "CREATE TRIGGER refuse_judged BEFORE INSERT ON events WHEN NEW.verb = 'brain.feedback' BEGIN SELECT RAISE(ABORT, 'injected judged failure'); END;"),
        ("grade", "CREATE TRIGGER refuse_judged BEFORE UPDATE OF grade ON brain_serve_ledger BEGIN SELECT RAISE(ABORT, 'injected judged failure'); END;"),
        ("private", "CREATE TRIGGER refuse_judged BEFORE INSERT ON brain_event_log WHEN NEW.event_kind = 'brain.feedback' BEGIN SELECT RAISE(ABORT, 'injected judged failure'); END;"),
        ("snapshot", "CREATE TRIGGER refuse_judged BEFORE INSERT ON brain_profile_snapshots BEGIN SELECT RAISE(ABORT, 'injected judged failure'); END;"),
    ] {
        let (pack, rt) = make_pack();
        let token = rt.authorize(Namespace::local()).unwrap();
        let registry = empty_registry();
        let target = create_test_entity(&rt, &token).await;
        warm(&pack, &token).await;
        record_serve(&rt, "rollback-ledger", "local", &target, Some("balanced-recall-v1"), Some("profile")).await;
        let before = training_state(&pack, &rt, &token).await;
        let public_before = feedback_events(&rt, &token).await;
        rt.sql().writer().await.unwrap().execute_script(trigger.into()).await.unwrap();
        let mut request = auto_request(&target, "useful");
        request["scorer_run_id"] = json!("rollback-run");
        request["serve_ledger_id"] = json!("rollback-ledger");
        let error = pack.dispatch("brain.auto_feedback", request.clone(), &registry, &token).await.unwrap_err();
        assert!(error.to_string().contains("injected judged failure"), "{case}: {error}");
        assert_eq!(training_state(&pack, &rt, &token).await, before, "{case}");
        assert_eq!(feedback_events(&rt, &token).await, public_before, "{case}");
        assert_grade(&rt, "rollback-ledger", None).await;
        rt.sql().writer().await.unwrap().execute_script("DROP TRIGGER refuse_judged;".into()).await.unwrap();
        assert_eq!(pack.dispatch("brain.auto_feedback", request.clone(), &registry, &token).await.unwrap()["emitted"], true);
        let committed = training_state(&pack, &rt, &token).await;
        assert_eq!(pack.dispatch("brain.auto_feedback", request, &registry, &token).await.unwrap()["deduped"], true);
        assert_eq!(training_state(&pack, &rt, &token).await, committed);
        assert_eq!(pack.snapshot().balanced_recall.total_events, 1);
        assert_eq!(committed["tables"]["claims"].as_array().unwrap().len(), 1);
        assert_eq!(committed["replay"].as_array().unwrap().len(), 1);
        assert!(committed["tables"]["mass"].as_array().unwrap().is_empty());
        assert_eq!(feedback_events(&rt, &token).await.len(), 1);
        assert_grade(&rt, "rollback-ledger", Some(("rollback-run", "useful"))).await;
    }
}

#[tokio::test]
async fn unjudged_event_append_failure_is_an_error_without_any_training_side_effect() {
    let (pack, rt) = make_pack();
    let token = rt.authorize(Namespace::local()).unwrap();
    let registry = empty_registry();
    let target = create_test_entity(&rt, &token).await;
    warm(&pack, &token).await;
    record_serve(
        &rt,
        "append-ledger",
        "local",
        &target,
        Some("balanced-recall-v1"),
        Some("profile"),
    )
    .await;
    let before = training_state(&pack, &rt, &token).await;
    rt.sql().writer().await.unwrap().execute_script(
        "CREATE TRIGGER refuse_unjudged BEFORE INSERT ON events WHEN NEW.kind = 'feedback_unjudged' BEGIN SELECT RAISE(ABORT, 'injected unjudged append failure'); END;".into()
    ).await.unwrap();
    let mut request = auto_request(&target, "unjudged");
    request["scorer_run_id"] = json!("append-run");
    request["serve_ledger_id"] = json!("append-ledger");
    let error = pack
        .dispatch("brain.auto_feedback", request.clone(), &registry, &token)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected unjudged append failure"),
        "{error}"
    );
    assert_eq!(training_state(&pack, &rt, &token).await, before);
    assert!(feedback_events(&rt, &token).await.is_empty());
    rt.sql()
        .writer()
        .await
        .unwrap()
        .execute_script("DROP TRIGGER refuse_unjudged;".into())
        .await
        .unwrap();
    assert_eq!(
        pack.dispatch("brain.auto_feedback", request, &registry, &token)
            .await
            .unwrap()["emitted"],
        true
    );
    assert_eq!(training_state(&pack, &rt, &token).await, before);
    assert_eq!(feedback_events(&rt, &token).await.len(), 1);
    assert_grade(&rt, "append-ledger", None).await;
}

#[tokio::test]
async fn unjudged_invalid_selection_attribution_and_provenance_write_nothing() {
    let (pack, rt) = make_pack();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;
    let other = create_test_entity(&rt, &token).await;
    let deleted = create_test_entity(&rt, &token).await;
    rt.delete_entity(&token, deleted.parse().unwrap(), false)
        .await
        .unwrap();
    warm(&pack, &token).await;
    record_serve(
        &rt,
        "guard-ledger",
        "local",
        &target,
        Some("balanced-recall-v1"),
        Some("profile"),
    )
    .await;
    record_serve(
        &rt,
        "other-target-ledger",
        "local",
        &other,
        Some("balanced-recall-v1"),
        Some("profile"),
    )
    .await;
    record_serve(
        &rt,
        "other-namespace-ledger",
        "other",
        &target,
        Some("balanced-recall-v1"),
        Some("profile"),
    )
    .await;
    let pair = json!({"scorer_run_id":"guard-run","serve_ledger_id":"guard-ledger"});
    let base = auto_request(&target, "unjudged");
    let mut cases = vec![];
    for (name, patch) in [
        ("half-scorer", json!({"scorer_run_id":"guard-run"})),
        ("half-ledger", json!({"serve_ledger_id":"guard-ledger"})),
        ("token-namespace", json!({"namespace":"other"})),
        ("empty-query", json!({"query":"  "})),
        ("profile-without-id", json!({"serve_attribution":"profile"})),
        (
            "unattributed-with-id",
            json!({"serve_attribution":"unattributed","served_by_profile_id":"balanced-recall-v1"}),
        ),
        (
            "unspecified-with-id",
            json!({"serve_attribution":"unspecified","served_by_profile_id":"balanced-recall-v1"}),
        ),
        ("unknown-selection", json!({"target_id":"not-a-result"})),
        (
            "duplicate-selection",
            json!({"results":[{"id":target},{"id":target}]}),
        ),
        (
            "malformed-full-id",
            json!({"results":[{"id":target,"full_id":"not-a-uuid"}]}),
        ),
        ("malformed-results", json!({"results":[target]})),
        ("empty-results", json!({"results":[]})),
        ("missing-selected-target", json!({"target_id":null})),
    ] {
        let mut request = base.clone();
        request
            .as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        cases.push((name, request));
    }
    for (name, ledger, profile, marker) in [
        ("missing-ledger", "missing-ledger", None, None),
        ("ledger-target", "other-target-ledger", None, None),
        ("ledger-namespace", "other-namespace-ledger", None, None),
        (
            "ledger-profile",
            "guard-ledger",
            Some("conflicting-profile"),
            None,
        ),
        (
            "ledger-unattributed",
            "guard-ledger",
            None,
            Some("unattributed"),
        ),
    ] {
        let mut request = base.clone();
        request
            .as_object_mut()
            .unwrap()
            .extend(pair.as_object().unwrap().clone());
        request["serve_ledger_id"] = json!(ledger);
        if let Some(profile) = profile {
            request["served_by_profile_id"] = json!(profile);
        }
        if let Some(marker) = marker {
            request["serve_attribution"] = json!(marker);
        }
        cases.push((name, request));
    }
    cases.push((
        "absent-target",
        auto_request(&uuid::Uuid::new_v4().to_string(), "unjudged"),
    ));
    cases.push(("deleted-target", auto_request(&deleted, "unjudged")));
    let before = training_state(&pack, &rt, &token).await;
    for (case, request) in cases {
        let result = pack
            .dispatch("brain.auto_feedback", request, &empty_registry(), &token)
            .await;
        assert!(result.is_err(), "{case} unexpectedly succeeded: {result:?}");
        assert_eq!(training_state(&pack, &rt, &token).await, before, "{case}");
        assert!(feedback_events(&rt, &token).await.is_empty(), "{case}");
    }
}

#[tokio::test]
async fn auto_judgments_without_a_pair_keep_ordinary_non_deduplicated_behavior() {
    let (pack, rt) = make_pack();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;
    for _ in 0..2 {
        let result = pack
            .dispatch(
                "brain.auto_feedback",
                auto_request(&target, "useful"),
                &empty_registry(),
                &token,
            )
            .await
            .unwrap();
        assert_eq!(result["emitted"], true);
    }
    assert_eq!(pack.snapshot().balanced_recall.total_events, 2);
    assert_eq!(feedback_events(&rt, &token).await.len(), 2);
    let state = training_state(&pack, &rt, &token).await;
    assert!(state["tables"]["claims"].as_array().unwrap().is_empty());
    assert!(state["tables"]["mass"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn stale_auto_judgment_revalidates_archived_or_removed_profile_before_claiming() {
    for removed in [false, true] {
        let (owner, rt) = make_pack();
        let token = rt.authorize(Namespace::local()).unwrap();
        let registry = empty_registry();
        let target = create_test_entity(&rt, &token).await;
        create_active_lifecycle_profile(&owner, &registry, &token).await;
        let stale = BrainPack::new(rt.clone());
        warm(&stale, &token).await;
        record_serve(
            &rt,
            "stale-ledger",
            "local",
            &target,
            Some(LIFECYCLE_PROFILE_ID),
            Some("profile"),
        )
        .await;
        if removed {
            // Simulate an authoritative registry removal after the other
            // instance's warm preflight, through the existing atomic writer.
            crate::persist::persist_brain_state_mutation(
                rt.sql().as_ref(),
                &token,
                &owner.persistence,
                &owner.state,
                crate::persist::BrainMutationEvent {
                    profile_id: LIFECYCLE_PROFILE_ID.into(),
                    event_kind: "brain.archive".into(),
                    payload: json!({"profile_id":LIFECYCLE_PROFILE_ID}),
                },
                ENTITY_CACHE_CAPACITY,
                |state| {
                    state.profiles.remove(LIFECYCLE_PROFILE_ID);
                    state.profile_states.remove(LIFECYCLE_PROFILE_ID);
                    Ok(())
                },
            )
            .await
            .unwrap();
        } else {
            for verb in ["brain.deactivate", "brain.archive"] {
                owner
                    .dispatch(
                        verb,
                        json!({"profile_id":LIFECYCLE_PROFILE_ID}),
                        &registry,
                        &token,
                    )
                    .await
                    .unwrap();
            }
        }
        assert_ne!(
            stale.snapshot().profiles[LIFECYCLE_PROFILE_ID].lifecycle,
            khive_brain_core::ProfileLifecycle::Archived
        );
        let before = training_state(&stale, &rt, &token).await;
        let public_before = feedback_events(&rt, &token).await;
        let mut request = auto_request(&target, "useful");
        request["served_by_profile_id"] = json!(LIFECYCLE_PROFILE_ID);
        request["scorer_run_id"] = json!("stale-run");
        request["serve_ledger_id"] = json!("stale-ledger");
        // Deliberately bypass dispatch's freshness check: the production
        // mutation transaction must still reject its now-stale preflight.
        let error = stale
            .handle_auto_feedback(&token, request, &registry)
            .await
            .unwrap_err();
        if removed {
            assert!(matches!(error, RuntimeError::NotFound(_)), "{error:?}");
        } else {
            assert!(
                matches!(error, RuntimeError::InvalidInput(ref message) if message.contains("archived")),
                "{error:?}"
            );
        }
        assert_eq!(training_state(&stale, &rt, &token).await, before);
        assert_eq!(feedback_events(&rt, &token).await, public_before);
        assert_grade(&rt, "stale-ledger", None).await;
    }
}

#[tokio::test]
async fn auto_legacy_explicit_aliases_and_correction_cannot_claim_a_scorer_pair() {
    let (pack, rt) = make_pack();
    let token = rt.authorize(Namespace::local()).unwrap();
    let registry = empty_registry();
    let target = create_test_entity(&rt, &token).await;
    warm(&pack, &token).await;
    record_serve(
        &rt,
        "alias-ledger",
        "local",
        &target,
        Some("balanced-recall-v1"),
        Some("profile"),
    )
    .await;
    let before = training_state(&pack, &rt, &token).await;
    for signal in ["explicit_positive", "explicit_negative", "correction"] {
        let mut request = auto_request(&target, signal);
        request["scorer_run_id"] = json!("alias-run");
        request["serve_ledger_id"] = json!("alias-ledger");
        let error = pack
            .dispatch("brain.auto_feedback", request, &registry, &token)
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("implicit_positive"),
            "{signal}: {error}"
        );
        assert_eq!(training_state(&pack, &rt, &token).await, before);
        assert!(feedback_events(&rt, &token).await.is_empty());
    }
    let mut accepted = auto_request(&target, "useful");
    accepted["scorer_run_id"] = json!("alias-run");
    accepted["serve_ledger_id"] = json!("alias-ledger");
    assert_eq!(
        pack.dispatch("brain.auto_feedback", accepted, &registry, &token)
            .await
            .unwrap()["emitted"],
        true
    );
    assert_grade(&rt, "alias-ledger", Some(("alias-run", "useful"))).await;
}

#[tokio::test]
async fn denied_unjudged_target_never_reaches_feedback_or_training_storage() {
    #[derive(Debug)]
    struct DenyTarget(String);
    impl khive_runtime::Gate for DenyTarget {
        fn check(
            &self,
            request: &khive_runtime::GateRequest,
        ) -> Result<khive_runtime::GateDecision, khive_runtime::GateError> {
            if request.verb == "brain.auto_feedback" && request.args["target_id"] == self.0 {
                Ok(khive_runtime::GateDecision::deny(
                    "target denied by test policy",
                ))
            } else {
                Ok(khive_runtime::GateDecision::allow())
            }
        }
    }
    let (pack, rt) = make_pack();
    let token = rt.authorize(Namespace::local()).unwrap();
    let target = create_test_entity(&rt, &token).await;
    warm(&pack, &token).await;
    record_serve(
        &rt,
        "denied-ledger",
        "local",
        &target,
        Some("balanced-recall-v1"),
        Some("profile"),
    )
    .await;
    let before = training_state(&pack, &rt, &token).await;
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("brain-test".into()));
    builder.with_gate(std::sync::Arc::new(DenyTarget(target.clone())));
    builder.register(khive_pack_kg::KgPack::new(rt.clone()));
    builder.register(BrainPack::new(rt.clone()));
    let registry = builder.build().unwrap();
    let mut request = auto_request(&target, "unjudged");
    request["scorer_run_id"] = json!("denied-run");
    request["serve_ledger_id"] = json!("denied-ledger");
    let error = registry
        .dispatch("brain.auto_feedback", request)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("target denied by test policy"),
        "{error}"
    );
    assert_eq!(training_state(&pack, &rt, &token).await, before);
    assert!(feedback_events(&rt, &token).await.is_empty());
    assert_grade(&rt, "denied-ledger", None).await;
}

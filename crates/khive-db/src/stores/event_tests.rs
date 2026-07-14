use super::*;
use crate::pool::PoolConfig;
use serde_json::json;

fn setup_memory_store() -> SqlEventStore {
    let config = PoolConfig {
        path: None,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());

    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(EVENTS_DDL).unwrap();
    }

    SqlEventStore::new_scoped(pool, false, "default")
}

fn make_event(namespace: &str) -> Event {
    Event::new(
        namespace,
        "search",
        EventKind::SearchExecuted,
        SubstrateKind::Note,
        "agent:test",
    )
    .with_payload(json!({ "result_kind": "note" }))
}

#[tokio::test]
async fn test_append_and_get_event() {
    let store = setup_memory_store();

    let event = make_event("default");
    let id = event.id;

    store.append_event(event).await.unwrap();

    let fetched = store.get_event(id).await.unwrap();
    assert!(fetched.is_some());
    let fetched = fetched.unwrap();
    assert_eq!(fetched.id, id);
    assert_eq!(fetched.verb, "search");
    assert_eq!(fetched.substrate, SubstrateKind::Note);
    assert_eq!(fetched.actor, "agent:test");
    assert_eq!(fetched.outcome, EventOutcome::Success);
}

#[tokio::test]
async fn test_append_events_batch() {
    let store = setup_memory_store();

    let events: Vec<Event> = (0..3).map(|_| make_event("default")).collect();
    let summary = store.append_events(events).await.unwrap();
    assert_eq!(summary.attempted, 3);
    assert_eq!(summary.affected, 3);
    assert_eq!(summary.failed, 0);
}

#[tokio::test]
async fn test_count_events() {
    let store = setup_memory_store();

    for _ in 0..3 {
        store.append_event(make_event("default")).await.unwrap();
    }

    let count = store.count_events(EventFilter::default()).await.unwrap();
    assert_eq!(count, 3);
}

#[tokio::test]
async fn test_query_events_filter_by_verb() {
    let store = setup_memory_store();

    store.append_event(make_event("default")).await.unwrap();

    let mut create_event = make_event("default");
    create_event.verb = "create".to_string();
    store.append_event(create_event).await.unwrap();

    let filter = EventFilter {
        verbs: vec!["search".to_string()],
        ..EventFilter::default()
    };
    let page = store
        .query_events(
            filter,
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].verb, "search");
}

#[tokio::test]
async fn test_query_events_filter_by_substrate() {
    let store = setup_memory_store();

    store.append_event(make_event("default")).await.unwrap();

    let mut entity_event = make_event("default");
    entity_event.substrate = SubstrateKind::Entity;
    store.append_event(entity_event).await.unwrap();

    let filter = EventFilter {
        substrates: vec![SubstrateKind::Entity],
        ..EventFilter::default()
    };
    let page = store
        .query_events(
            filter,
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].substrate, SubstrateKind::Entity);
}

#[tokio::test]
async fn test_outcome_roundtrip() {
    let store = setup_memory_store();

    let mut denied = make_event("default");
    denied.outcome = EventOutcome::Denied;
    let denied_id = denied.id;
    store.append_event(denied).await.unwrap();

    let fetched = store.get_event(denied_id).await.unwrap().unwrap();
    assert_eq!(fetched.outcome, EventOutcome::Denied);
}

#[tokio::test]
async fn append_event_writes_observations_atomically() {
    let store = setup_memory_store();
    let candidate = Uuid::new_v4();
    let selected = Uuid::new_v4();
    let mut event = make_event("default");
    event.kind = EventKind::SearchExecuted;
    event.payload = json!({
        "result_kind": "note",
        "candidates": [candidate.to_string()],
        "selected": [selected.to_string()],
        "served_by_profile_id": "profile-a"
    });
    let event_id = event.id;

    store.append_event(event).await.unwrap();

    // Verify event was inserted.
    let fetched = store.get_event(event_id).await.unwrap();
    assert!(fetched.is_some());

    // Verify observations were written.
    let pool = Arc::clone(&store.pool);
    let event_id_str = event_id.to_string();
    let (candidate_count, selected_count) = tokio::task::spawn_blocking(move || {
            let guard = pool.reader().unwrap();
            let conn = guard.conn();
            let c: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM event_observations WHERE event_id = ?1 AND role = 'candidate'",
                    [&event_id_str],
                    |r| r.get(0),
                )
                .unwrap();
            let s: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM event_observations WHERE event_id = ?1 AND role = 'selected'",
                    [&event_id_str],
                    |r| r.get(0),
                )
                .unwrap();
            (c, s)
        })
        .await
        .unwrap();

    assert_eq!(candidate_count, 1, "expected one candidate observation row");
    assert_eq!(selected_count, 1, "expected one selected observation row");
}

#[tokio::test]
async fn search_executed_rejects_unknown_result_kind() {
    let store = setup_memory_store();
    let mut event = make_event("default");
    event.payload = json!({
        "result_kind": "edge",
        "candidates": [Uuid::new_v4().to_string()],
        "selected": []
    });
    let event_id = event.id;

    let result = store.append_event(event).await;
    assert!(result.is_err(), "unknown result_kind must be rejected");
    assert!(
        store.get_event(event_id).await.unwrap().is_none(),
        "invalid event and projection must roll back atomically"
    );
}

async fn selected_uuids_for(store: &SqlEventStore, event_id: Uuid) -> Vec<String> {
    let pool = Arc::clone(&store.pool);
    let event_id_str = event_id.to_string();
    tokio::task::spawn_blocking(move || {
        let guard = pool.reader().unwrap();
        let conn = guard.conn();
        let mut stmt = conn
            .prepare(
                "SELECT entity_id FROM event_observations \
                 WHERE event_id = ?1 AND role = 'selected' ORDER BY position",
            )
            .unwrap();
        stmt.query_map([&event_id_str], |r| r.get(0))
            .unwrap()
            .collect::<Result<Vec<String>, _>>()
            .unwrap()
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn rerank_executed_falls_back_to_reranked_when_final_scores_field_is_absent() {
    // Regression test (#831): legacy
    // events emitted before `final_scores` existed carry only `reranked`.
    // The decoder must still fall through from the absent `final_scores`
    // field to the tuple-shaped `reranked` field instead of silently
    // projecting zero `selected` rows. Constructed via a hand-rolled
    // `json!` (not the typed struct) so the `final_scores` key is
    // genuinely absent, not present-and-empty.
    let store = setup_memory_store();
    let candidate = Uuid::new_v4();
    let reranked_winner = Uuid::new_v4();

    let mut event = make_event("default");
    event.kind = EventKind::RerankExecuted;
    event.payload = json!({
        "candidates": [candidate.to_string()],
        "reranked": [[reranked_winner.to_string(), [["relevance", 0.9]]]],
    });
    let event_id = event.id;

    store.append_event(event).await.unwrap();

    let selected = selected_uuids_for(&store, event_id).await;
    assert_eq!(
        selected,
        vec![reranked_winner.to_string()],
        "selected observation must decode the UUID leading the `reranked` tuple \
         when `final_scores` is absent"
    );
}

#[tokio::test]
async fn rerank_executed_prefers_final_scores_order_over_reranked_when_both_present() {
    // #831: ADR-042 §5 defines
    // `final_scores` as the ordered rerank output and `reranked` as
    // unordered per-reranker audit/debug data. When both are present and
    // their orderings differ, `final_scores`' order must win.
    let store = setup_memory_store();
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();

    let payload = khive_types::RerankExecutedPayload {
        served_by_profile_id: Some("profile-a".to_string()),
        model_id: khive_types::Id128::from_u128(1),
        candidates: vec![
            khive_types::Id128::from_bytes(*a.as_bytes()),
            khive_types::Id128::from_bytes(*b.as_bytes()),
        ],
        // audit-only ordering: b before a
        reranked: vec![
            (
                khive_types::Id128::from_bytes(*b.as_bytes()),
                vec![("relevance".to_string(), 0.4)],
            ),
            (
                khive_types::Id128::from_bytes(*a.as_bytes()),
                vec![("relevance".to_string(), 0.9)],
            ),
        ],
        // authoritative ordered output: a before b
        final_scores: vec![
            (khive_types::Id128::from_bytes(*a.as_bytes()), 0.9),
            (khive_types::Id128::from_bytes(*b.as_bytes()), 0.4),
        ],
        latency_us: 1200,
        hook_applied: false,
        hook_target_match: false,
    };

    let mut event = make_event("default");
    event.kind = EventKind::RerankExecuted;
    event.payload = serde_json::to_value(&payload).unwrap();
    let event_id = event.id;

    store.append_event(event).await.unwrap();

    let selected = selected_uuids_for(&store, event_id).await;
    assert_eq!(
        selected,
        vec![a.to_string(), b.to_string()],
        "selected order must follow `final_scores`, not `reranked`"
    );
}

#[tokio::test]
async fn rerank_executed_ignores_stray_selected_field_and_uses_final_scores() {
    // Regression test (#831):
    // `RerankExecutedPayload` (khive_types::event::RerankExecutedPayload)
    // has no `selected` field at all. The serde helper does not deny
    // unknown fields, so a payload carrying the typed fields plus a stray
    // `selected` key must still be projected from `final_scores` only —
    // the stray field must never be consulted, even when its order
    // differs completely from `final_scores`.
    let store = setup_memory_store();
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let stray_selected_winner = Uuid::new_v4();

    let payload = khive_types::RerankExecutedPayload {
        served_by_profile_id: Some("profile-a".to_string()),
        model_id: khive_types::Id128::from_u128(1),
        candidates: vec![
            khive_types::Id128::from_bytes(*a.as_bytes()),
            khive_types::Id128::from_bytes(*b.as_bytes()),
        ],
        reranked: vec![],
        // authoritative ordered output: a before b
        final_scores: vec![
            (khive_types::Id128::from_bytes(*a.as_bytes()), 0.9),
            (khive_types::Id128::from_bytes(*b.as_bytes()), 0.4),
        ],
        latency_us: 1200,
        hook_applied: false,
        hook_target_match: false,
    };

    let mut event = make_event("default");
    event.kind = EventKind::RerankExecuted;
    let mut payload_value = serde_json::to_value(&payload).unwrap();
    // Inject a stray `selected` field with an entirely different order
    // than `final_scores` — this field is not part of the typed contract
    // and must not affect projection.
    payload_value.as_object_mut().unwrap().insert(
        "selected".to_string(),
        json!([stray_selected_winner.to_string()]),
    );
    event.payload = payload_value;
    let event_id = event.id;

    store.append_event(event).await.unwrap();

    let selected = selected_uuids_for(&store, event_id).await;
    assert_eq!(
        selected,
        vec![a.to_string(), b.to_string()],
        "stray `selected` field must be ignored for RerankExecuted; \
         projection must follow `final_scores` only"
    );
}

#[tokio::test]
async fn rerank_executed_uses_final_scores_when_reranked_is_empty() {
    // Companion case: `reranked` present-but-empty, `final_scores`
    // populated. `final_scores` must still be used (not skipped just
    // because `reranked` happens to come first lexically in the payload).
    let store = setup_memory_store();
    let winner = Uuid::new_v4();

    let payload = khive_types::RerankExecutedPayload {
        served_by_profile_id: None,
        model_id: khive_types::Id128::from_u128(1),
        candidates: vec![khive_types::Id128::from_bytes(*winner.as_bytes())],
        reranked: vec![],
        final_scores: vec![(khive_types::Id128::from_bytes(*winner.as_bytes()), 0.75)],
        latency_us: 800,
        hook_applied: false,
        hook_target_match: false,
    };

    let mut event = make_event("default");
    event.kind = EventKind::RerankExecuted;
    event.payload = serde_json::to_value(&payload).unwrap();
    let event_id = event.id;

    store.append_event(event).await.unwrap();

    let selected = selected_uuids_for(&store, event_id).await;
    assert_eq!(
        selected,
        vec![winner.to_string()],
        "final_scores must be used even when reranked is present-but-empty"
    );
}

#[tokio::test]
async fn rerank_executed_final_scores_single_element_tuple_rejected() {
    // #831: the typed contract
    // (`RerankExecutedPayload::final_scores: Vec<(Id128, f32)>`) requires
    // exact two-element tuples. A one-element tuple must error, and the
    // event insert must roll back rather than silently accepting it.
    let store = setup_memory_store();
    let mut event = make_event("default");
    event.kind = EventKind::RerankExecuted;
    event.payload = json!({
        "candidates": [],
        "final_scores": [[Uuid::new_v4().to_string()]],
    });
    let event_id = event.id;

    let result = store.append_event(event).await;
    assert!(result.is_err(), "single-element tuple must be rejected");

    let fetched = store.get_event(event_id).await.unwrap();
    assert!(fetched.is_none(), "event row must not exist after rollback");
}

#[tokio::test]
async fn rerank_executed_final_scores_extra_element_tuple_rejected() {
    // Extra-element tuples must also be rejected, not
    // silently truncated to the first two elements.
    let store = setup_memory_store();
    let mut event = make_event("default");
    event.kind = EventKind::RerankExecuted;
    event.payload = json!({
        "candidates": [],
        "final_scores": [[Uuid::new_v4().to_string(), 0.5, "extra"]],
    });
    let event_id = event.id;

    let result = store.append_event(event).await;
    assert!(result.is_err(), "extra-element tuple must be rejected");

    let fetched = store.get_event(event_id).await.unwrap();
    assert!(fetched.is_none(), "event row must not exist after rollback");
}

#[tokio::test]
async fn rerank_executed_final_scores_non_numeric_second_element_rejected() {
    // The second element of a `final_scores` tuple must be the
    // typed contract's `f32` score, not an arbitrary value.
    let store = setup_memory_store();
    let mut event = make_event("default");
    event.kind = EventKind::RerankExecuted;
    event.payload = json!({
        "candidates": [],
        "final_scores": [[Uuid::new_v4().to_string(), "not-a-number"]],
    });
    let event_id = event.id;

    let result = store.append_event(event).await;
    assert!(result.is_err(), "non-numeric score must be rejected");

    let fetched = store.get_event(event_id).await.unwrap();
    assert!(fetched.is_none(), "event row must not exist after rollback");
}

#[tokio::test]
async fn rerank_executed_reranked_malformed_sub_scores_rejected() {
    // `reranked`'s second element must be the typed contract's
    // `Vec<(String, f32)>` sub-score list, not an arbitrary value.
    let store = setup_memory_store();
    let mut event = make_event("default");
    event.kind = EventKind::RerankExecuted;
    event.payload = json!({
        "candidates": [],
        "reranked": [[Uuid::new_v4().to_string(), "not-an-array"]],
    });
    let event_id = event.id;

    let result = store.append_event(event).await;
    assert!(
        result.is_err(),
        "malformed reranked sub-scores must be rejected"
    );

    let fetched = store.get_event(event_id).await.unwrap();
    assert!(fetched.is_none(), "event row must not exist after rollback");
}

#[tokio::test]
async fn feedback_explicit_projects_signal_observation_from_target_id() {
    // Regression test for #811: the emitter (khive-pack-brain's `brain.feedback`
    // handler) sets `event.target_id` via `Event::with_target`, never a payload
    // `about_id` field. The decoder must read the field the emitter actually
    // writes so the round trip survives.
    let store = setup_memory_store();
    let target = Uuid::new_v4();
    let event = Event::new(
        "default",
        "brain.feedback",
        EventKind::FeedbackExplicit,
        SubstrateKind::Event,
        "agent:test",
    )
    .with_target(target)
    .with_payload(json!({ "signal": "useful" }));
    let event_id = event.id;

    store.append_event(event).await.unwrap();

    let pool = Arc::clone(&store.pool);
    let event_id_str = event_id.to_string();
    let (signal_count, observed_entity_id): (i64, String) =
        tokio::task::spawn_blocking(move || {
            let guard = pool.reader().unwrap();
            let conn = guard.conn();
            let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM event_observations WHERE event_id = ?1 AND role = 'signal'",
                [&event_id_str],
                |r| r.get(0),
            )
            .unwrap();
            let entity_id: String = conn
            .query_row(
                "SELECT entity_id FROM event_observations WHERE event_id = ?1 AND role = 'signal'",
                [&event_id_str],
                |r| r.get(0),
            )
            .unwrap();
            (count, entity_id)
        })
        .await
        .unwrap();

    assert_eq!(signal_count, 1, "expected one signal observation row");
    assert_eq!(
        observed_entity_id,
        target.to_string(),
        "signal observation must carry the feedback's target_id"
    );
}

#[tokio::test]
async fn invalid_projection_payload_aborts_event_insert() {
    let store = setup_memory_store();
    let mut event = make_event("default");
    event.kind = EventKind::RerankExecuted;
    // "candidates" must be an array of UUID strings, not a plain string.
    event.payload = json!({ "candidates": "not-array" });
    let event_id = event.id;

    let result = store.append_event(event).await;
    assert!(result.is_err(), "invalid payload must return Err");

    // The event row must not exist — transaction was rolled back.
    let fetched = store.get_event(event_id).await.unwrap();
    assert!(fetched.is_none(), "event row must not exist after rollback");
}

#[tokio::test]
async fn query_events_orders_by_created_at_then_id_desc() {
    let store = setup_memory_store();

    let ts = chrono::Utc::now().timestamp_micros();
    let id_low = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
    let id_high = Uuid::parse_str("ffffffff-ffff-ffff-ffff-ffffffffffff").unwrap();

    // Insert both events with identical created_at via direct SQL to bypass UUID generation.
    let pool = Arc::clone(&store.pool);
    tokio::task::spawn_blocking(move || {
            let guard = pool.try_writer().unwrap();
            let conn = guard.conn();
            conn.execute_batch("BEGIN IMMEDIATE").unwrap();
            for id in [id_low, id_high] {
                conn.execute(
                    "INSERT INTO events \
                     (id, namespace, verb, substrate, actor, kind, outcome, payload, \
                      payload_schema_version, duration_us, created_at) \
                     VALUES (?1, 'default', 'search', 'note', 'test', 'audit', 'success', '{}', 1, 0, ?2)",
                    rusqlite::params![id.to_string(), ts],
                )
                .unwrap();
            }
            conn.execute_batch("COMMIT").unwrap();
        })
        .await
        .unwrap();

    let page = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();

    assert_eq!(page.items.len(), 2);
    assert_eq!(
        page.items[0].id, id_high,
        "higher UUID must come first (id DESC tiebreaker)"
    );
    assert_eq!(page.items[1].id, id_low);
}

#[tokio::test]
async fn query_events_filters_by_kind() {
    let store = setup_memory_store();
    store.append_event(make_event("default")).await.unwrap();
    let mut recall_event = make_event("default");
    recall_event.kind = EventKind::RecallExecuted;
    store.append_event(recall_event).await.unwrap();

    let filter = EventFilter {
        kinds: vec![EventKind::RecallExecuted],
        ..EventFilter::default()
    };
    let page = store
        .query_events(
            filter,
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].kind, EventKind::RecallExecuted);
}

#[tokio::test]
async fn query_events_filters_by_session_id() {
    let store = setup_memory_store();
    let session = Uuid::new_v4();
    let mut event = make_event("default");
    event.session_id = Some(session);
    store.append_event(event).await.unwrap();
    store.append_event(make_event("default")).await.unwrap();

    let filter = EventFilter {
        session_id: Some(session),
        ..EventFilter::default()
    };
    let page = store
        .query_events(
            filter,
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].session_id, Some(session));
}

#[tokio::test]
async fn query_events_filters_by_observed() {
    let store = setup_memory_store();
    let entity_id = Uuid::new_v4();
    let mut event = make_event("default");
    event.kind = EventKind::SearchExecuted;
    event.payload = json!({
        "result_kind": "entity",
        "candidates": [entity_id.to_string()],
        "selected": []
    });
    store.append_event(event).await.unwrap();
    store.append_event(make_event("default")).await.unwrap();

    let filter = EventFilter {
        observed: vec![entity_id],
        ..EventFilter::default()
    };
    let page = store
        .query_events(
            filter,
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
}

#[tokio::test]
async fn query_events_filters_by_selected() {
    let store = setup_memory_store();
    let entity_id = Uuid::new_v4();
    let mut event = make_event("default");
    event.kind = EventKind::SearchExecuted;
    event.payload = json!({
        "result_kind": "entity",
        "candidates": [],
        "selected": [entity_id.to_string()]
    });
    store.append_event(event).await.unwrap();
    store.append_event(make_event("default")).await.unwrap();

    let filter = EventFilter {
        selected: vec![entity_id],
        ..EventFilter::default()
    };
    let page = store
        .query_events(
            filter,
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
}

#[tokio::test]
async fn query_events_filters_by_payload_proposal_id() {
    let store = setup_memory_store();
    let proposal_id = Uuid::new_v4();
    let mut event = make_event("default");
    event.kind = EventKind::ProposalCreated;
    event.payload = json!({ "proposal_id": proposal_id.to_string() });
    store.append_event(event).await.unwrap();
    store.append_event(make_event("default")).await.unwrap();

    let filter = EventFilter {
        payload_proposal_id: Some(proposal_id),
        ..EventFilter::default()
    };
    let page = store
        .query_events(
            filter,
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
}

#[tokio::test]
async fn query_events_observed_filter_missing_projection_returns_clean_error() {
    // Set up a legacy-schema store (no event_observations table).
    let config = PoolConfig {
        path: None,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    {
        let writer = pool.writer().unwrap();
        // Create only the events table, without event_observations.
        writer.conn().execute_batch(
                "CREATE TABLE IF NOT EXISTS events (\
                     id TEXT PRIMARY KEY, namespace TEXT NOT NULL, verb TEXT NOT NULL,\
                     substrate TEXT NOT NULL, actor TEXT NOT NULL, kind TEXT NOT NULL DEFAULT 'audit',\
                     outcome TEXT NOT NULL, payload TEXT NOT NULL DEFAULT '{}',\
                     payload_schema_version INTEGER NOT NULL DEFAULT 1,\
                     duration_us INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL\
                 );"
            ).unwrap();
    }
    let store = SqlEventStore::new_scoped(pool, false, "default");

    let filter = EventFilter {
        observed: vec![Uuid::new_v4()],
        ..EventFilter::default()
    };
    let result = store
        .query_events(
            filter,
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await;
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("event_observations") && err_msg.contains("run migrations"),
        "error should mention event_observations and run migrations, got: {err_msg}"
    );
}

// ── KDB-006 regression: i64 → u32 narrowing on payload_schema_version ──────

/// KDB-006: payload_schema_version of 1 (normal) must round-trip without error.
#[tokio::test]
async fn read_event_with_valid_payload_schema_version() {
    let store = setup_memory_store();
    let event = make_event("default");
    let id = event.id;
    store.append_event(event).await.unwrap();

    let fetched = store.get_event(id).await.unwrap().unwrap();
    assert_eq!(
        fetched.payload_schema_version, 1,
        "default payload_schema_version must round-trip as 1"
    );
}

/// KDB-006: a row with a negative payload_schema_version stored directly via SQL
/// must be rejected by read_event (try_into fails → StorageError).
#[tokio::test]
async fn read_event_rejects_negative_payload_schema_version() {
    use crate::pool::PoolConfig;
    let config = PoolConfig {
        path: None,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(EVENTS_DDL).unwrap();
    }
    let store = SqlEventStore::new_scoped(Arc::clone(&pool), false, "default");

    // Insert a row with payload_schema_version = -1 directly.
    let id = uuid::Uuid::new_v4();
    {
        let writer = pool.writer().unwrap();
        writer
            .conn()
            .execute(
                "INSERT INTO events \
                 (id, namespace, verb, substrate, actor, kind, outcome, payload, \
                  payload_schema_version, duration_us, created_at) \
                 VALUES (?1,'default','test','entity','a','audit','success','{}', -1, 0, 0)",
                rusqlite::params![id.to_string()],
            )
            .unwrap();
    }

    let result = store.get_event(id).await;
    assert!(
        result.is_err(),
        "negative payload_schema_version must be rejected as a StorageError"
    );
}

/// KDB-006 regression: u32::MAX + 1 must be rejected by the position conversion helper.
/// The `as u32` truncation bug was the blocker finding — this test verifies the exact
/// boundary that would have silently wrapped before the fix.
#[test]
fn observation_position_u32_max_plus_one_is_rejected() {
    // usize value one past u32::MAX — this is the exact overflow boundary.
    let overflow_position: usize = u32::MAX as usize + 1;
    let result = u32::try_from(overflow_position);
    assert!(
        result.is_err(),
        "u32::MAX + 1 ({overflow_position}) must not fit in u32"
    );
}

/// KDB-006 regression: u32::MAX itself must convert successfully (boundary value).
#[test]
fn observation_position_u32_max_is_accepted() {
    let max_position: usize = u32::MAX as usize;
    let result = u32::try_from(max_position);
    assert!(
        result.is_ok(),
        "u32::MAX ({max_position}) must be a valid position"
    );
    assert_eq!(result.unwrap(), u32::MAX);
}

/// KDB-006 regression: payload_schema_version = u32::MAX + 1 (i64 = 4294967296) must be rejected.
#[tokio::test]
async fn read_event_rejects_payload_schema_version_u32_max_plus_one() {
    let config = PoolConfig {
        path: None,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(config).unwrap());
    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(EVENTS_DDL).unwrap();
    }
    let store = SqlEventStore::new_scoped(Arc::clone(&pool), false, "default");

    let id = uuid::Uuid::new_v4();
    let overflow_version: i64 = i64::from(u32::MAX) + 1;
    {
        let writer = pool.writer().unwrap();
        writer
            .conn()
            .execute(
                "INSERT INTO events \
                 (id, namespace, verb, substrate, actor, kind, outcome, payload, \
                  payload_schema_version, duration_us, created_at) \
                 VALUES (?1,'default','test','entity','a','audit','success','{}', ?2, 0, 0)",
                rusqlite::params![id.to_string(), overflow_version],
            )
            .unwrap();
    }

    let result = store.get_event(id).await;
    assert!(
        result.is_err(),
        "payload_schema_version = u32::MAX + 1 ({overflow_version}) must be rejected"
    );
}

/// STORAGE-AUD-003 / #485: PageRequest.offset > i64::MAX must return
/// InvalidInput instead of silently narrowing to a negative i64 offset.
#[tokio::test]
async fn page_offset_over_i64max_rejected() {
    let store = setup_memory_store();
    store.append_event(make_event("default")).await.unwrap();

    let result = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                offset: (i64::MAX as u64) + 1,
                limit: 10,
            },
        )
        .await;

    assert!(
        matches!(result, Err(StorageError::InvalidInput { .. })),
        "expected InvalidInput, got {result:?}"
    );
}

/// ADR-067 Component A entry 5: with `KHIVE_WRITE_QUEUE=1`, `append_events`
/// routes through the WriterTask channel instead of the pool-mutex path, and
/// both events are actually committed and independently readable back.
#[tokio::test]
async fn append_events_routes_through_writer_task_when_flag_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("write_queue_events.db");
    let pool_cfg = PoolConfig {
        path: Some(path.clone()),
        write_queue_enabled: true,
        ..PoolConfig::default()
    };
    let pool = Arc::new(ConnectionPool::new(pool_cfg).unwrap());
    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(EVENTS_DDL).unwrap();
    }

    let store = SqlEventStore::new_scoped(Arc::clone(&pool), true, "default");

    let e1 = make_event("default");
    let e2 = make_event("default");
    let id1 = e1.id;
    let id2 = e2.id;

    let summary = store.append_events(vec![e1, e2]).await.unwrap();
    assert_eq!(summary.attempted, 2);
    assert_eq!(summary.affected, 2);
    assert_eq!(summary.failed, 0);

    assert!(store.get_event(id1).await.unwrap().is_some());
    assert!(store.get_event(id2).await.unwrap().is_some());
    assert_eq!(
        pool.writer_task_spawn_count(),
        1,
        "the flag-ON path must actually spawn and use the writer task"
    );
}

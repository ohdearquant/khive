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
async fn append_usage_mark_distinguishes_typed_writer_outcomes() {
    use khive_storage::usage::{scope, UsageContext, UsageUnit};

    for state in [
        WriterTaskRequestState::NotStarted,
        WriterTaskRequestState::TransactionRolledBack,
        WriterTaskRequestState::SideEffectsUnknown,
    ] {
        for error in [
            StorageError::writer_task_terminated(state),
            StorageError::WriterTaskRequestFailed {
                request_state: state,
                source: Box::new(StorageError::Pool {
                    operation: "append_event".into(),
                    message: "write failed".into(),
                }),
            },
        ] {
            let ctx = UsageContext::new();
            ctx.add(UsageUnit::EventRows, 1);
            scope(ctx.clone(), async { mark_unknown_append_usage(&error) }).await;
            assert_eq!(
                ctx.shipping_snapshot().is_none(),
                state == WriterTaskRequestState::SideEffectsUnknown,
                "{error:?}"
            );
            assert_eq!(ctx.snapshot()["event_rows"], 1);
        }
    }

    let ctx = UsageContext::new();
    scope(ctx.clone(), async {
        mark_unknown_append_usage(&StorageError::driver(
            StorageCapability::Events,
            "append_event",
            std::io::Error::other("write failed"),
        ));
    })
    .await;
    assert_eq!(ctx.shipping_snapshot(), Some(json!({})));
    mark_unknown_append_usage(&StorageError::writer_task_terminated(
        WriterTaskRequestState::SideEffectsUnknown,
    ));
}

async fn observations_for(store: &SqlEventStore, event_id: Uuid) -> Vec<EventObservation> {
    let pool = Arc::clone(&store.pool);
    tokio::task::spawn_blocking(move || {
        let guard = pool.reader().unwrap();
        fetch_event_observations(guard.conn(), event_id).unwrap()
    })
    .await
    .unwrap()
}

include!("event_tests/operation_attribution.rs");

#[tokio::test]
async fn profile_state_version_refuses_overflow_before_any_event_is_persisted() {
    let store = setup_memory_store();
    let candidate = Uuid::new_v4();
    let mut valid = make_event("default").with_profile_state_version(i64::MAX as u64);
    valid.payload = json!({
        "result_kind": "note",
        "candidates": [candidate.to_string()],
    });
    let overflow = make_event("default").with_profile_state_version(i64::MAX as u64 + 1);

    assert_eq!(event_insert_statements(&valid).unwrap().len(), 2);
    assert!(event_insert_statements(&overflow).is_err());
    assert!(store.preflight_event(&overflow).is_err());
    assert!(store.append_event(overflow.clone()).await.is_err());
    assert!(store
        .append_events(vec![valid.clone(), overflow.clone()])
        .await
        .is_err());
    assert!(observations_for(&store, valid.id).await.is_empty());
    assert!(store
        .append_events_idempotent(vec![valid.clone(), overflow.clone()])
        .await
        .is_err());
    assert_eq!(store.count_events(EventFilter::default()).await.unwrap(), 0);
    assert!(observations_for(&store, valid.id).await.is_empty());

    store.append_event(valid.clone()).await.unwrap();
    assert_eq!(observations_for(&store, valid.id).await.len(), 1);
    assert_eq!(
        store.get_event(valid.id).await.unwrap(),
        Some(valid.clone())
    );
    let mut invalid_retry = valid;
    invalid_retry.profile_state_version = overflow.profile_state_version;
    assert!(store
        .append_events_idempotent(vec![invalid_retry])
        .await
        .is_err());
    assert_eq!(store.count_events(EventFilter::default()).await.unwrap(), 1);
    assert_eq!(
        store
            .query_events(
                EventFilter::default(),
                PageRequest {
                    limit: 10,
                    offset: 0
                }
            )
            .await
            .unwrap()
            .items
            .len(),
        1
    );
}

#[test]
fn profile_state_version_builder_binds_input_value() {
    let event = make_event("default").with_profile_state_version(41);
    let statements = event_insert_statements(&event).unwrap();
    assert_eq!(statements.len(), 1);
    match statements[0].params.get(9) {
        Some(SqlValue::Integer(value)) => assert_eq!(*value, 41),
        other => panic!("expected bound profile_state_version 41, got {other:?}"),
    }
}

#[tokio::test]
async fn prepared_event_insert_stores_and_reads_i64_max_profile_state_version() {
    let store = setup_memory_store();
    let event = make_event("default").with_profile_state_version(i64::MAX as u64);
    let statements = event_insert_statements(&event).expect("boundary value is representable");

    {
        let writer = store.pool.writer().unwrap();
        let conn = writer.conn();
        for statement in statements {
            let mut prepared = conn.prepare(&statement.sql).unwrap();
            crate::sql_bridge::bind_params(&mut prepared, &statement.params).unwrap();
            assert_eq!(prepared.raw_execute().unwrap(), 1);
        }
        let stored: i64 = conn
            .query_row(
                "SELECT profile_state_version FROM events WHERE id = ?1",
                [event.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, i64::MAX);
    }

    assert_eq!(store.get_event(event.id).await.unwrap(), Some(event));
}

#[tokio::test]
async fn batch_failure_rolls_back_event_and_observation_rows() {
    let store = setup_memory_store();
    let candidate = Uuid::new_v4();
    let mut first = make_event("default");
    first.payload = json!({
        "result_kind": "note",
        "candidates": [candidate.to_string()],
    });
    assert_eq!(decode_event_observations(&first).unwrap().len(), 1);

    let mut rejected = make_event("default");
    rejected.payload = json!({"result_kind": "edge"});
    assert!(store
        .append_events(vec![first.clone(), rejected.clone()])
        .await
        .is_err());
    assert_eq!(store.count_events(EventFilter::default()).await.unwrap(), 0);
    assert!(observations_for(&store, first.id).await.is_empty());
    assert!(observations_for(&store, rejected.id).await.is_empty());
}

#[tokio::test]
async fn profile_state_version_audit_finds_preexisting_unreadable_rows() {
    let store = setup_memory_store();
    let valid_null = make_event("default");
    let valid_max = make_event("default").with_profile_state_version(i64::MAX as u64);
    let negative = make_event("default");
    let text = make_event("default");
    let real = make_event("default");
    for event in [
        valid_null.clone(),
        valid_max.clone(),
        negative.clone(),
        text.clone(),
        real.clone(),
    ] {
        store.append_event(event).await.unwrap();
    }

    // Simulate rows written before the profile-version guard existed.
    {
        let writer = store.pool.writer().unwrap();
        let conn = writer.conn();
        for (id, value) in [
            (negative.id, rusqlite::types::Value::Integer(-1)),
            (
                text.id,
                rusqlite::types::Value::Text("not-an-integer".into()),
            ),
            (real.id, rusqlite::types::Value::Real(1.5)),
        ] {
            assert_eq!(
                conn.execute(
                    "UPDATE events SET profile_state_version = ?1 WHERE id = ?2",
                    rusqlite::params![value, id.to_string()],
                )
                .unwrap(),
                1,
            );
        }
    }

    let rows: Vec<(String, String, String, String)> = {
        let reader = store.pool.reader().unwrap();
        let mut statement = reader
            .conn()
            .prepare(include_str!(
                "../../docs/api/event-profile-state-version-audit.sql"
            ))
            .unwrap();
        let found = statement
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        found
    };
    let mut expected = vec![
        (
            negative.id.to_string(),
            "default".to_string(),
            "integer".to_string(),
            "-1".to_string(),
        ),
        (
            text.id.to_string(),
            "default".to_string(),
            "text".to_string(),
            "'not-an-integer'".to_string(),
        ),
        (
            real.id.to_string(),
            "default".to_string(),
            "real".to_string(),
            "1.5".to_string(),
        ),
    ];
    expected.sort_by(|left, right| left.0.cmp(&right.0));
    assert_eq!(rows, expected);
    assert_eq!(
        store.get_event(valid_null.id).await.unwrap(),
        Some(valid_null)
    );
    assert_eq!(
        store.get_event(valid_max.id).await.unwrap(),
        Some(valid_max)
    );
    for id in [negative.id, text.id, real.id] {
        assert!(store.get_event(id).await.is_err());
    }
    assert!(store
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .is_err());
}

include!("event_tests/baseline.rs");

include!("event_tests/search_projection.rs");

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

include!("event_tests/rerank_projection.rs");

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

include!("event_tests/query_and_link.rs");

include!("event_tests/schema_bounds.rs");

/// ADR-067 Component A entry 5: with `KHIVE_WRITE_QUEUE=1`, `append_events`
/// routes through the WriterTask channel instead of the pool-mutex path, and
/// both events are actually committed and independently readable back.
#[tokio::test]
async fn append_events_routes_through_writer_task_when_flag_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("write_queue_events.db");
    let pool_cfg = PoolConfig {
        path: Some(path.clone()),
        write_queue_enabled: Some(true),
        ..PoolConfig::for_test()
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

fn malformed_recall_event(namespace: &str) -> Event {
    Event::new(
        namespace,
        "recall",
        EventKind::RecallExecuted,
        SubstrateKind::Note,
        "agent:test",
    )
    .with_payload(json!({ "candidates": "not-an-array" }))
}

#[tokio::test]
async fn preflight_rejects_malformed_observation_before_enqueue() {
    let store = setup_memory_store();

    let bad = malformed_recall_event("default");
    let bad_id = bad.id;
    let result = store.preflight_event(&bad);
    assert!(
        result.is_err(),
        "preflight must reject a payload the observation decoder cannot build statements for"
    );

    // Preflight performs no I/O: the row must never have been enqueued.
    assert!(store.get_event(bad_id).await.unwrap().is_none());

    // A well-formed event still preflights clean.
    let good = make_event("default");
    assert!(store.preflight_event(&good).is_ok());
}

include!("event_tests/idempotent_batches.rs");

#[tokio::test]
async fn query_events_declines_to_compute_a_total() {
    let store = setup_memory_store();

    for _ in 0..3 {
        store.append_event(make_event("default")).await.unwrap();
    }

    // The paged read returns no total: the unconditional `COUNT(*)` it used to
    // run carried no `LIMIT`, so it scanned the whole filtered set on every
    // page while the data query fetched only `offset + limit` rows. `total` is
    // `Option<u64>` so a store may decline it, and no caller of this path reads
    // it.
    let page = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 2,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(page.total, None);

    // The page itself is unaffected: the limit still applies and the rows are
    // still returned. This is the arm that fails if the change removed more
    // than the count.
    assert_eq!(page.items.len(), 2);

    // A caller that genuinely wants a cardinality still has one, and it sees
    // all three rows rather than the page's two — so `None` above is a
    // declined count, not a broken query.
    let counted = store.count_events(EventFilter::default()).await.unwrap();
    assert_eq!(counted, 3);
}

#[tokio::test]
async fn refusal_target_filter_keeps_query_count_namespace_and_observations_separate() {
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: None,
            write_queue_enabled: Some(false),
            write_routing_strict: false,
            ..PoolConfig::default()
        })
        .unwrap(),
    );
    pool.writer()
        .unwrap()
        .conn()
        .execute_batch(EVENTS_DDL)
        .unwrap();
    let store = SqlEventStore::new_scoped(Arc::clone(&pool), false, "default");
    let other = SqlEventStore::new_scoped(pool, false, "other");
    let subject = Uuid::new_v4();
    let refusal = |namespace: &str, target| {
        Event::new(
            namespace,
            "knowledge.upsert_atoms",
            EventKind::Refusal,
            SubstrateKind::Event,
            "actor:refusal-test",
        )
        .with_target(target)
        .with_outcome(EventOutcome::Denied)
        .with_payload(json!({"subject_kind": "knowledge_atom"}))
    };
    let first = refusal("default", subject);
    let second = refusal("default", subject);
    store
        .append_events(vec![
            first.clone(),
            second.clone(),
            refusal("default", Uuid::new_v4()),
            Event::new(
                "default",
                "knowledge.upsert_atoms",
                EventKind::Refusal,
                SubstrateKind::Event,
                "actor:refusal-test",
            ),
            Event::new(
                "default",
                "test",
                EventKind::Audit,
                SubstrateKind::Event,
                "actor:refusal-test",
            )
            .with_target(subject),
        ])
        .await
        .unwrap();
    other.append_event(refusal("other", subject)).await.unwrap();
    let filter = EventFilter {
        target_id: Some(subject),
        kinds: vec![EventKind::Refusal],
        ..EventFilter::default()
    };
    assert_eq!(store.count_events(filter.clone()).await.unwrap(), 2);
    let mut found = Vec::new();
    for offset in 0..2 {
        let page = store
            .query_events(filter.clone(), PageRequest { limit: 1, offset })
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].target_id, Some(subject));
        assert_eq!(page.items[0].namespace, "default");
        found.push(page.items[0].id);
    }
    found.sort();
    let mut expected = vec![first.id, second.id];
    expected.sort();
    assert_eq!(found, expected);
    assert_eq!(other.count_events(filter.clone()).await.unwrap(), 1);
    assert_eq!(
        store
            .count_events(EventFilter {
                target_id: Some(subject),
                ..EventFilter::default()
            })
            .await
            .unwrap(),
        3
    );
    let observations = EventFilter {
        observed: vec![subject],
        ..filter
    };
    assert_eq!(store.count_events(observations.clone()).await.unwrap(), 0);
    assert!(
        store
            .query_events(
                observations,
                PageRequest {
                    limit: 10,
                    offset: 0
                }
            )
            .await
            .unwrap()
            .items
            .is_empty(),
        "knowledge subjects must not become graph observations"
    );
}

#[tokio::test]
async fn weighted_rerank_without_model_projects_candidates_and_selected() {
    let store = setup_memory_store();
    let a = khive_types::Id128::from_u128(71);
    let b = khive_types::Id128::from_u128(72);
    let payload = khive_types::RerankExecutedPayload {
        reranker: khive_types::RerankerKind::Weighted,
        served_by_profile_id: None,
        model_id: None,
        query_id: Some("query/session-73".into()),
        tiers: vec!["relevance".into()],
        ignored_weights: vec!["unknown".into()],
        unidentified_candidates: 1,
        candidates: vec![a, b],
        // A candidate whose scores do not fit f32 still has a candidate row.
        reranked: vec![(b, vec![("relevance".into(), 0.75)])],
        final_scores: vec![(b, 0.75)],
        latency_us: 20,
        hook_applied: false,
        hook_target_match: false,
    };
    let value = serde_json::to_value(&payload).unwrap();
    assert!(value.get("model_id").is_none());
    let event = Event::new(
        "default",
        "memory.recall_rerank",
        EventKind::RerankExecuted,
        SubstrateKind::Note,
        "tester",
    )
    .with_payload(value);
    let id = event.id;
    let observations = decode_event_observations(&event).unwrap();
    let expected = [
        (a, ObservationRole::Candidate, 0),
        (b, ObservationRole::Candidate, 1),
        (b, ObservationRole::Selected, 0),
    ];
    assert_eq!(observations.len(), expected.len());
    for (observation, (entity, role, position)) in observations.iter().zip(expected) {
        assert_eq!(observation.event_id, id);
        assert_eq!(observation.entity_id, Uuid::from_bytes(*entity.as_bytes()));
        assert_eq!(observation.referent_kind, ReferentKind::Note);
        assert_eq!(observation.role, role);
        assert_eq!(observation.position, position);
    }
    store.append_event(event).await.unwrap();
    assert_eq!(selected_uuids_for(&store, id).await, vec![b.to_string()]);
    let stored = store.get_event(id).await.unwrap().unwrap();
    assert_eq!(
        serde_json::from_value::<khive_types::RerankExecutedPayload>(stored.payload).unwrap(),
        payload
    );
}

#[tokio::test]
async fn legacy_v4_event_identity_survives_append_get_and_paged_query() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy_event_identity.db");
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(path),
            write_queue_enabled: Some(true),
            ..PoolConfig::for_test()
        })
        .unwrap(),
    );
    {
        let writer = pool.writer().unwrap();
        writer.conn().execute_batch(EVENTS_DDL).unwrap();
    }
    let store = SqlEventStore::new_scoped(pool, true, "default");

    let legacy_id = Uuid::parse_str("f47ac10b-58cc-4372-a567-0e02b2c3d479").unwrap();
    assert_eq!(legacy_id.get_version_num(), 4);
    let mut legacy = make_event("default");
    legacy.id = legacy_id;
    legacy.created_at = 10;
    store.append_event(legacy.clone()).await.unwrap();

    let mut current = make_event("default");
    assert_eq!(current.id.get_version_num(), 7);
    current.created_at = 20;
    store.append_event(current.clone()).await.unwrap();

    let mut excluded = make_event("default");
    excluded.actor = "agent:excluded".into();
    excluded.created_at = 30;
    store.append_event(excluded).await.unwrap();

    assert_eq!(
        store.get_event(legacy_id).await.unwrap(),
        Some(legacy.clone())
    );
    assert_eq!(
        store.get_event(current.id).await.unwrap(),
        Some(current.clone())
    );

    let filter = EventFilter {
        actors: vec!["agent:test".into()],
        kinds: vec![EventKind::SearchExecuted],
        ..EventFilter::default()
    };
    let expected = vec![current, legacy.clone()];
    let queried = store
        .query_events(
            filter.clone(),
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(queried.items, expected);

    for (offset, event) in (0..2).zip(expected) {
        let page = store
            .query_events(filter.clone(), PageRequest { limit: 1, offset })
            .await
            .unwrap();
        assert_eq!(page.items, vec![event]);
    }

    let by_legacy_id = store
        .query_events(
            EventFilter {
                ids: vec![legacy_id],
                ..EventFilter::default()
            },
            PageRequest {
                limit: 1,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(by_legacy_id.items, vec![legacy]);
}

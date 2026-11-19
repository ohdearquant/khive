#[tokio::test]
async fn query_events_orders_by_created_at_then_id_desc() {
    let store = setup_memory_store();

    let ts = 1_700_000_000_000_000_i64;
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

    let newer_low = Uuid::from_u128(2);
    let newer_high = Uuid::from_u128(3);
    let older_low = Uuid::from_u128(4);
    let older_high = Uuid::from_u128(5);
    let rows = [
        (older_high, ts - 10),
        (newer_low, ts + 10),
        (older_low, ts - 10),
        (newer_high, ts + 10),
    ];
    let events = rows
        .into_iter()
        .map(|(id, created_at)| {
            let mut event = make_event("default");
            event.id = id;
            event.created_at = created_at;
            event
        })
        .collect();
    let written = store.append_events(events).await.unwrap();
    assert_eq!(written.attempted, 4);
    assert_eq!(written.affected, 4);
    assert_eq!(written.failed, 0);
    let expected = [
        newer_high, newer_low, id_high, id_low, older_high, older_low,
    ];
    let whole = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                offset: 0,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        whole.items.iter().map(|event| event.id).collect::<Vec<_>>(),
        expected
    );

    for limit in [1_u32, 2, 3, 4] {
        let mut seen = Vec::new();
        for offset in (0..expected.len()).step_by(limit as usize) {
            let page = store
                .query_events(
                    EventFilter::default(),
                    PageRequest {
                        offset: offset as u64,
                        limit,
                    },
                )
                .await
                .unwrap();
            let ids: Vec<_> = page.items.iter().map(|event| event.id).collect();
            let end = (offset + limit as usize).min(expected.len());
            assert_eq!(ids, expected[offset..end], "offset={offset} limit={limit}");
            seen.extend(ids);
        }
        assert_eq!(seen, expected, "complete pagination at limit={limit}");
        let terminal = store
            .query_events(
                EventFilter::default(),
                PageRequest {
                    offset: expected.len() as u64,
                    limit,
                },
            )
            .await
            .unwrap();
        assert!(terminal.items.is_empty(), "terminal page at limit={limit}");
    }
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
async fn link_events_project_edge_referents_and_observed_matches_any_role() {
    let store = setup_memory_store();
    let edge_id = Uuid::new_v4();
    let source_id = Uuid::new_v4();
    let target_id = Uuid::new_v4();
    let event = Event::new(
        "default",
        "link",
        EventKind::LinkCreated,
        SubstrateKind::Entity,
        "agent:test",
    )
    .with_target(edge_id)
    .with_payload(json!({
        "id": edge_id,
        "source_id": source_id,
        "target_id": target_id,
        "source_kind": "entity",
        "target_kind": "entity",
        "mutation": "created"
    }));
    let event_id = event.id;
    let mut legacy = event.clone();
    legacy
        .payload
        .as_object_mut()
        .unwrap()
        .remove("source_kind");
    legacy
        .payload
        .as_object_mut()
        .unwrap()
        .remove("target_kind");
    assert_eq!(
        decode_link_observations(&event).unwrap(),
        decode_link_observations(&legacy).unwrap(),
        "explicit entity endpoints must preserve today's observation bytes"
    );
    store.append_event(event).await.unwrap();

    for observed in [source_id, target_id, edge_id] {
        let page = store
            .query_events(
                EventFilter {
                    observed: vec![observed],
                    ..EventFilter::default()
                },
                PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].id, event_id);
    }

    let rows = observations_for(&store, event_id).await;
    assert_eq!(rows.len(), 3);
    for (position, id, kind) in [
        (0, source_id, ReferentKind::Entity),
        (1, target_id, ReferentKind::Entity),
        (2, edge_id, ReferentKind::Edge),
    ] {
        assert_eq!(rows[position].entity_id, id);
        assert_eq!(rows[position].referent_kind, kind);
        assert_eq!(rows[position].role, ObservationRole::Target);
        assert_eq!(rows[position].position, position as u32);
    }
}

#[tokio::test]
async fn link_note_endpoints_project_note_referents() {
    let store = setup_memory_store();
    let source_id = Uuid::new_v4();
    let target_id = Uuid::new_v4();
    let edge_id = Uuid::new_v4();
    let event = Event::new(
        "default",
        "link",
        EventKind::LinkCreated,
        SubstrateKind::Entity,
        "agent:test",
    )
    .with_target(edge_id)
    .with_payload(json!({
        "id": edge_id,
        "source_id": source_id,
        "target_id": target_id,
        "source_kind": "note",
        "target_kind": "note",
        "relation": "supports"
    }));
    let event_id = event.id;
    store.append_event(event).await.unwrap();

    let rows = observations_for(&store, event_id).await;
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].entity_id, source_id);
    assert_eq!(rows[0].referent_kind, ReferentKind::Note);
    assert_eq!(rows[1].entity_id, target_id);
    assert_eq!(rows[1].referent_kind, ReferentKind::Note);
    assert_eq!(rows[2].entity_id, edge_id);
    assert_eq!(rows[2].referent_kind, ReferentKind::Edge);
}

#[tokio::test]
async fn legacy_link_payload_replays_with_entity_endpoint_fallback() {
    let store = setup_memory_store();
    let source_id = Uuid::new_v4();
    let target_id = Uuid::new_v4();
    let edge_id = Uuid::new_v4();
    let event = Event::new(
        "default",
        "link",
        EventKind::LinkCreated,
        SubstrateKind::Entity,
        "agent:test",
    )
    .with_target(edge_id)
    .with_payload(json!({
        "id": edge_id,
        "source_id": source_id,
        "target_id": target_id
    }));
    let first = store
        .append_events_idempotent(vec![event.clone()])
        .await
        .unwrap();
    assert_eq!(first.rows, vec![EventAppendDisposition::Inserted]);
    let rows = observations_for(&store, event.id).await;
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].referent_kind, ReferentKind::Entity);
    assert_eq!(rows[1].referent_kind, ReferentKind::Entity);
    assert_eq!(rows[2].referent_kind, ReferentKind::Edge);

    let retry = store.append_events_idempotent(vec![event]).await.unwrap();
    assert_eq!(
        retry.rows,
        vec![EventAppendDisposition::AlreadyPresentIdentical]
    );
}

#[tokio::test]
async fn link_to_event_projects_edge_without_event_endpoint() {
    let store = setup_memory_store();
    let source_id = Uuid::new_v4();
    let target_event_id = Uuid::new_v4();
    let edge_id = Uuid::new_v4();
    let event = Event::new(
        "default",
        "link",
        EventKind::LinkCreated,
        SubstrateKind::Entity,
        "agent:test",
    )
    .with_target(edge_id)
    .with_payload(json!({
        "id": edge_id,
        "source_id": source_id,
        "target_id": target_event_id,
        "source_kind": "note",
        "target_kind": "event",
        "relation": "annotates"
    }));
    let event_id = event.id;
    store.append_event(event).await.unwrap();

    let rows = observations_for(&store, event_id).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].entity_id, source_id);
    assert_eq!(rows[0].referent_kind, ReferentKind::Note);
    assert_eq!(rows[0].position, 0);
    assert_eq!(rows[1].entity_id, edge_id);
    assert_eq!(rows[1].referent_kind, ReferentKind::Edge);
    assert_eq!(rows[1].position, 2);
    assert!(rows.iter().all(|row| row.entity_id != target_event_id));
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

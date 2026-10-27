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
async fn get_event_by_id_is_namespace_agnostic() {
    let source = setup_memory_store();
    let target = SqlEventStore::new_scoped(Arc::clone(&source.pool), false, "target");
    let event = make_event("default").with_payload(json!({
        "result_kind": "note",
        "text": "the source event survives a lookup through the target handle",
    }));
    source.append_event(event.clone()).await.unwrap();

    assert_eq!(
        source.get_event(event.id).await.unwrap(),
        Some(event.clone())
    );
    assert_eq!(target.get_event(event.id).await.unwrap(), Some(event));
    assert!(target.get_event(Uuid::new_v4()).await.unwrap().is_none());
    // Multi-record queries retain the namespace bound to their store handle.
    let target_page = target
        .query_events(EventFilter::default(), PageRequest::default())
        .await
        .unwrap();
    assert!(target_page.items.is_empty());
    assert_eq!(
        target.count_events(EventFilter::default()).await.unwrap(),
        0
    );
    assert_eq!(
        source.count_events(EventFilter::default()).await.unwrap(),
        1
    );
}

#[tokio::test]
async fn fixed_v4_event_identity_survives_append_get_and_queries() {
    let store = setup_memory_store();
    // Explicit legacy IDs: this guard never calls the current event constructor.
    let legacy_id = Uuid::from_u128(0x12345678_1234_4abc_8123_123456789abc);
    let legacy = Event {
        id: legacy_id,
        namespace: "default".into(),
        verb: "search".into(),
        substrate: SubstrateKind::Note,
        actor: "agent:legacy".into(),
        kind: EventKind::SearchExecuted,
        outcome: EventOutcome::Success,
        payload: json!({"result_kind": "note", "legacy": true}),
        payload_schema_version: 1,
        profile_state_version: None,
        duration_us: 17,
        target_id: None,
        session_id: None,
        aggregate_kind: None,
        aggregate_id: None,
        created_at: 1_700_000_000_000_000,
        op_index: None,
        ref_resolution: None,
    };
    let mut other = legacy.clone();
    other.id = Uuid::from_u128(0x87654321_4321_4def_9234_abcdef012345);
    other.payload = json!({"result_kind": "note", "legacy": false});
    store.append_event(legacy.clone()).await.unwrap();
    store.append_event(other.clone()).await.unwrap();

    assert_eq!(
        store.get_event(legacy_id).await.unwrap(),
        Some(legacy.clone())
    );
    let all = store
        .query_events(
            EventFilter::default(),
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(all.items.len(), 2);
    assert!(all.items.contains(&legacy));
    assert!(all.items.contains(&other));
    // A same-namespace, same-kind/verb decoy makes an ignored ID filter observable.
    let selected = store
        .query_events(
            EventFilter {
                ids: vec![legacy_id],
                ..EventFilter::default()
            },
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(selected.items, vec![legacy]);
}

use super::*;

fn refusal(namespace: &str, target: Uuid) -> Event {
    Event::new(
        namespace,
        "knowledge.upsert_atoms",
        EventKind::Refusal,
        SubstrateKind::Event,
        "actor:event-target-test",
    )
    .with_target(target)
}

#[tokio::test]
async fn target_query_rejects_pre_filter_protocol_before_opening_a_store() {
    let backend = StorageBackend::memory().unwrap();
    let stores: NamespaceStores = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let subject = Uuid::new_v4();
    let request = EventsRequest::QueryEvents {
        protocol_version: 3,
        namespace: "local".into(),
        filter: EventFilter {
            target_id: Some(subject),
            ..EventFilter::default()
        },
        page: PageRequest {
            limit: 10,
            offset: 0,
        },
    };
    let wire = serde_json::to_vec(&request).unwrap();
    let decoded = serde_json::from_slice(&wire).unwrap();
    let response = dispatch_events_request(decoded, &backend, &stores).await;
    match response {
        EventsResponse::Error {
            message, retryable, ..
        } => {
            assert!(message.contains("protocol version mismatch"));
            assert!(!retryable);
        }
        other => panic!("old peers must not silently ignore target_id: {other:?}"),
    }
    assert!(
        stores.lock().unwrap().is_empty(),
        "version mismatch must fail before opening a namespace store"
    );
}

#[tokio::test]
async fn exact_target_filter_crosses_socket_and_merges_both_event_stores() {
    let dir = tempfile::tempdir().unwrap();
    let _registry_guard = TestRegistryGuard::new(dir.path());
    let (_db, socket) = boot_daemon(&dir).await;
    let client = EventsSplitClient::new(socket).unwrap();
    let lane: Arc<dyn EventStore> =
        Arc::new(ForwardingEventStore::new("local", Arc::clone(&client)));
    let other_lane = ForwardingEventStore::new("other", client);
    let legacy_backend = direct_backend_for(&dir.path().join("legacy.db")).unwrap();
    let legacy = legacy_backend.events_for_namespace("local").unwrap();
    let split = SplitEventStore::new(Arc::clone(&legacy), Arc::clone(&lane));
    let subject = Uuid::new_v4();
    let local = refusal("local", subject);
    let remote = refusal("local", subject);
    split.append_event(local.clone()).await.unwrap();
    split
        .append_events_idempotent(vec![remote.clone(), refusal("local", Uuid::new_v4())])
        .await
        .unwrap();
    legacy
        .append_event(refusal("local", Uuid::new_v4()))
        .await
        .unwrap();
    other_lane
        .append_events_idempotent(vec![refusal("other", subject)])
        .await
        .unwrap();
    let filter = EventFilter {
        target_id: Some(subject),
        kinds: vec![EventKind::Refusal],
        ..EventFilter::default()
    };
    assert_eq!(legacy.count_events(filter.clone()).await.unwrap(), 1);
    assert_eq!(lane.count_events(filter.clone()).await.unwrap(), 1);
    assert_eq!(split.count_events(filter.clone()).await.unwrap(), 2);
    let mut ids = Vec::new();
    for offset in 0..2 {
        let page = split
            .query_events(filter.clone(), PageRequest { limit: 1, offset })
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].target_id, Some(subject));
        assert_eq!(page.items[0].namespace, "local");
        ids.push(page.items[0].id);
    }
    ids.sort();
    let mut expected = vec![local.id, remote.id];
    expected.sort();
    assert_eq!(ids, expected);
    let observed = EventFilter {
        observed: vec![subject],
        ..filter
    };
    assert_eq!(
        split.count_events(observed).await.unwrap(),
        0,
        "atom subjects do not create graph observations"
    );
}

#[tokio::test]
async fn grouped_counts_cross_socket_and_merge_without_row_paging() {
    let dir = tempfile::tempdir().unwrap();
    let _registry_guard = TestRegistryGuard::new(dir.path());
    let (_db, socket) = boot_daemon(&dir).await;
    let client = EventsSplitClient::new(socket).unwrap();
    let lane: Arc<dyn EventStore> =
        Arc::new(ForwardingEventStore::new("local", Arc::clone(&client)));
    let foreign = ForwardingEventStore::new("foreign", client);
    let backend = direct_backend_for(&dir.path().join("legacy.db")).unwrap();
    let legacy = backend.events_for_namespace("local").unwrap();
    let split = SplitEventStore::new(Arc::clone(&legacy), Arc::clone(&lane));
    let make = |namespace: &str, verb: &str, actor: &str, at| {
        let mut event = Event::new(
            namespace,
            verb,
            EventKind::Audit,
            SubstrateKind::Note,
            actor,
        );
        event.created_at = at;
        event
    };
    split
        .append_events(vec![
            make("local", "shared", "a", 101),
            make("local", "main", "b", 102),
        ])
        .await
        .unwrap();
    lane.append_events_idempotent(vec![
        make("local", "shared", "a", 103),
        make("local", "lane", "c", 104),
        make("local", "excluded", "a", 200),
    ])
    .await
    .unwrap();
    foreign
        .append_events_idempotent(vec![make("foreign", "hidden", "a", 105)])
        .await
        .unwrap();
    let filter = EventFilter {
        after: Some(100),
        before: Some(200),
        ..Default::default()
    };
    assert_eq!(
        split
            .count_events_grouped(filter.clone(), EventGroupBy::Verb)
            .await
            .unwrap(),
        BTreeMap::from([("shared".into(), 2), ("main".into(), 1), ("lane".into(), 1)])
    );
    assert_eq!(
        split
            .count_events_grouped(filter.clone(), EventGroupBy::Kind)
            .await
            .unwrap(),
        BTreeMap::from([("audit".into(), 4)])
    );
    assert_eq!(
        split
            .count_events_grouped(filter.clone(), EventGroupBy::Actor)
            .await
            .unwrap(),
        BTreeMap::from([("a".into(), 2), ("b".into(), 1), ("c".into(), 1)])
    );
    assert!(split
        .count_events_grouped(
            EventFilter {
                verbs: vec!["absent".into()],
                ..filter
            },
            EventGroupBy::Verb
        )
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn grouped_count_protocol_rejects_old_version_before_opening_store() {
    let backend = StorageBackend::memory().unwrap();
    let stores: NamespaceStores = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    for request in [
        EventsRequest::CountEvents {
            protocol_version: 4,
            namespace: "local".into(),
            filter: EventFilter::default(),
        },
        EventsRequest::CountEventsGrouped {
            protocol_version: 4,
            namespace: "local".into(),
            filter: EventFilter::default(),
            group_by: EventGroupBy::Verb,
        },
    ] {
        let request = serde_json::from_slice(&serde_json::to_vec(&request).unwrap()).unwrap();
        match dispatch_events_request(request, &backend, &stores).await {
            EventsResponse::Error {
                message, retryable, ..
            } => {
                assert!(message.contains("protocol version mismatch"));
                assert!(!retryable);
            }
            other => panic!("old count protocol must refuse: {other:?}"),
        }
    }
    assert!(stores.lock().unwrap().is_empty());
}

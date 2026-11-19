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

#[tokio::test]
async fn search_executed_absent_result_kind_projects_historical_note_rows() {
    let store = setup_memory_store();
    let mut event = make_event("default");
    let note_id = Uuid::new_v4();
    event.payload = json!({
        "candidates": [note_id.to_string()],
        "selected": [note_id.to_string()]
    });
    let event_id = event.id;
    store.preflight_event(&event).unwrap();
    event_insert_statements(&event).unwrap();
    store.append_event(event.clone()).await.unwrap();
    let event_id_str = event_id.to_string();
    let reader = store.pool.reader().unwrap();
    let mut stmt = reader
        .conn()
        .prepare(
            "SELECT entity_id, referent_kind, role FROM event_observations \
             WHERE event_id = ?1 ORDER BY role, position",
        )
        .unwrap();
    let rows = stmt
        .query_map([&event_id_str], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![
            (note_id.to_string(), "note".into(), "candidate".into()),
            (note_id.to_string(), "note".into(), "selected".into()),
        ]
    );
    drop(stmt);
    drop(reader);
    let replay = store.append_events_idempotent(vec![event]).await.unwrap();
    assert_eq!(
        replay.rows,
        vec![EventAppendDisposition::AlreadyPresentIdentical]
    );
}

#[tokio::test]
async fn search_executed_rejects_non_object_payload_root() {
    let store = setup_memory_store();
    for payload in [json!("not an object"), json!([]), json!(null)] {
        let mut event = make_event("default");
        event.payload = payload;
        let event_id = event.id;

        assert!(store.preflight_event(&event).is_err());
        assert!(event_insert_statements(&event).is_err());
        assert!(store.append_event(event).await.is_err());
        assert!(store.get_event(event_id).await.unwrap().is_none());
    }
}

#[tokio::test]
async fn search_executed_rejects_present_non_string_result_kind() {
    let store = setup_memory_store();
    for result_kind in [json!(null), json!(0), json!(["note"])] {
        let mut event = make_event("default");
        event.payload = json!({"result_kind": result_kind, "candidates": [], "selected": []});
        let event_id = event.id;
        assert!(store.preflight_event(&event).is_err());
        assert!(event_insert_statements(&event).is_err());
        assert!(store.append_event(event).await.is_err());
        assert!(store.get_event(event_id).await.unwrap().is_none());
    }
}

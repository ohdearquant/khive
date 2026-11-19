#[tokio::test]
async fn note_supports_link_endpoints_are_observed_as_target() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let source = rt
        .create_note(&tok, "observation", None, "source", None, None, vec![])
        .await
        .unwrap();
    let target = rt
        .create_note(&tok, "insight", None, "target", None, None, vec![])
        .await
        .unwrap();
    rt.link(
        &tok,
        source.id,
        target.id,
        EdgeRelation::Supports,
        1.0,
        None,
    )
    .await
    .unwrap();

    let link_events = rt
        .list_events(
            &tok,
            EventFilter {
                kinds: vec![EventKind::LinkCreated],
                ..EventFilter::default()
            },
            PageRequest::default(),
        )
        .await
        .unwrap();
    assert_eq!(link_events.items.len(), 1);
    let link_event = &link_events.items[0];
    assert_eq!(link_event.payload["source_kind"], "note");
    assert_eq!(link_event.payload["target_kind"], "note");

    let query = format!(
        "MATCH (ev)-[:observed_as_target]->(t) WHERE ev.id = '{}' RETURN t.id",
        link_event.id
    );
    let rows = rt.query(&tok, &query).await.unwrap();
    let observed_ids: BTreeSet<_> = rows
        .iter()
        .flat_map(|row| row.columns.iter())
        .filter_map(|column| match &column.value {
            khive_storage::types::SqlValue::Text(value) => Some(value.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        observed_ids,
        BTreeSet::from([source.id.to_string(), target.id.to_string()])
    );
}

#[tokio::test]
async fn annotates_event_target_has_no_phantom_target_observation() {
    let rt = rt();
    let tok = rt.authorize(Namespace::local()).unwrap();
    let note = rt
        .create_note(&tok, "observation", None, "annotation", None, None, vec![])
        .await
        .unwrap();
    let note_events = rt
        .list_events(
            &tok,
            EventFilter {
                kinds: vec![EventKind::NoteCreated],
                ..EventFilter::default()
            },
            PageRequest::default(),
        )
        .await
        .unwrap();
    assert_eq!(note_events.items.len(), 1);
    let event_target = note_events.items[0].id;
    rt.link(
        &tok,
        note.id,
        event_target,
        EdgeRelation::Annotates,
        1.0,
        None,
    )
    .await
    .unwrap();

    let link_events = rt
        .list_events(
            &tok,
            EventFilter {
                kinds: vec![EventKind::LinkCreated],
                ..EventFilter::default()
            },
            PageRequest::default(),
        )
        .await
        .unwrap();
    assert_eq!(link_events.items.len(), 1);
    let link_event = &link_events.items[0];
    assert_eq!(link_event.payload["source_kind"], "note");
    assert_eq!(link_event.payload["target_kind"], "event");

    let query = format!(
        "MATCH (ev)-[:observed_as_target]->(t) WHERE ev.id = '{}' RETURN t.id",
        link_event.id
    );
    let rows = rt.query(&tok, &query).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].columns.iter().any(|column| {
        matches!(&column.value, khive_storage::types::SqlValue::Text(value) if value == &note.id.to_string())
    }));
}

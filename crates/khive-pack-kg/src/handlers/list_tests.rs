use super::parse_after_cursor;
use super::{add_list_limit_metadata, overfetch_limit, render_list_response, split_overfetched};
use crate::handlers::common::{event_filter_from_params, ListParams};
use crate::sql::sql;

/// The defect this field exists for: at `limit == cap` the three older
/// disclosure fields are identical for a complete page and a truncated one.
#[test]
fn a_full_page_at_the_cap_is_distinguishable_from_a_complete_one() {
    let truncated = render_list_response(serde_json::json!([]), 500, 500, true);
    let complete = render_list_response(serde_json::json!([]), 500, 500, false);

    for page in [&truncated, &complete] {
        assert_eq!(page["requested_limit"], 500);
        assert_eq!(page["effective_limit"], 500);
        assert_eq!(page["limit_clamped"], false);
    }
    assert_ne!(
        truncated, complete,
        "a truncated page and a complete one must not serialize identically"
    );
    assert_eq!(truncated["has_more"], true);
    assert_eq!(complete["has_more"], false);
}

/// The two truncation causes are independent: the cap clamp is something a
/// caller can act on, the population is something it can only page through.
#[test]
fn the_cap_clamp_and_the_population_are_reported_separately() {
    let clamped_and_complete = render_list_response(serde_json::json!([]), 600, 500, false);
    assert_eq!(clamped_and_complete["limit_clamped"], true);
    assert_eq!(clamped_and_complete["has_more"], false);

    let unclamped_and_truncated = render_list_response(serde_json::json!([]), 10, 10, true);
    assert_eq!(unclamped_and_truncated["limit_clamped"], false);
    assert_eq!(unclamped_and_truncated["has_more"], true);
}

#[test]
fn the_added_metadata_form_carries_the_same_pair() {
    let mut response = serde_json::json!({"entities": [], "next_after": null});
    add_list_limit_metadata(&mut response, 500, 500, true);
    assert_eq!(response["limit_clamped"], false);
    assert_eq!(response["has_more"], true);
    assert_eq!(response["entities"], serde_json::json!([]));
}

#[test]
fn the_over_fetched_row_is_never_returned_and_is_the_whole_signal() {
    let exactly_full: Vec<u32> = (0..5).collect();
    let (page, has_more) = split_overfetched(exactly_full, 5);
    assert_eq!(page.len(), 5);
    assert!(!has_more, "a page the store could not extend is complete");

    let one_over: Vec<u32> = (0..6).collect();
    let (page, has_more) = split_overfetched(one_over, 5);
    assert_eq!(page, vec![0, 1, 2, 3, 4], "the extra row must not escape");
    assert!(has_more);

    let short: Vec<u32> = (0..2).collect();
    let (page, has_more) = split_overfetched(short, 5);
    assert_eq!(page.len(), 2);
    assert!(!has_more);
}

#[test]
fn the_over_fetch_asks_for_exactly_one_more_and_cannot_overflow() {
    assert_eq!(overfetch_limit(0), 1);
    assert_eq!(overfetch_limit(500), 501);
    assert_eq!(overfetch_limit(u32::MAX), u32::MAX);
}

#[test]
fn after_cursor_rejects_prefix_with_keyset_consequence() {
    let error = parse_after_cursor("deadbeef").expect_err("prefix is not an exact cursor");
    let message = error.to_string();
    assert!(message.contains("can miss or be ambiguous"), "{message}");
    assert!(
        message.contains("exact stable insertion boundary"),
        "{message}"
    );
}

#[test]
fn event_filters_reject_prefixes_with_exact_record_consequence() {
    for (field, value) in [
        ("session_id", serde_json::json!("deadbeef")),
        ("observed", serde_json::json!(["deadbeef"])),
        ("selected", serde_json::json!(["deadbeef"])),
    ] {
        let mut args = serde_json::json!({"kind": "event"});
        args[field] = value;
        let params: ListParams = serde_json::from_value(args).expect("list params");
        let error = event_filter_from_params(&params)
            .expect_err("prefix is not an exact event-filter identifier");
        let message = error.to_string();
        assert!(message.contains(field), "{message}");
        assert!(message.contains("can miss or be ambiguous"), "{message}");
        assert!(message.contains("exact stable record"), "{message}");
    }
}

#[tokio::test]
async fn message_thread_scope_matches_a_routing_field_only_as_text() {
    use khive_runtime::{KhiveRuntime, Namespace};
    use khive_storage::types::{SqlStatement, SqlValue};

    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let namespace = Namespace::parse("scope-one").expect("valid namespace");
    let token = runtime.authorize(namespace).expect("authorized namespace");
    for (thread, to_actor) in [
        (
            "aaaaaaaa-bbbb-4ccc-8ddd-000000000001",
            serde_json::json!("x"),
        ),
        (
            "aaaaaaaa-bbbb-4ccc-8ddd-000000000002",
            serde_json::json!(["x"]),
        ),
    ] {
        runtime
            .create_note(
                &token,
                "message",
                None,
                "routed message",
                None,
                Some(serde_json::json!({
                    "thread_id": thread,
                    "direction": "inbound",
                    "to_actor": to_actor,
                })),
                vec![],
            )
            .await
            .expect("create message note");
    }
    // An array routing field renders as `["x"]` under json_extract; an
    // actor id spelling that text must not be admitted to the thread.
    for (actor, expected) in [
        ("x", vec!["aaaaaaaa-bbbb-4ccc-8ddd-000000000001"]),
        (r#"["x"]"#, vec![]),
    ] {
        let mut reader = runtime.sql().reader().await.expect("SQL reader");
        let rows = reader
            .query_all(SqlStatement {
                sql: sql!("message_threads_list").to_string(),
                params: vec![
                    SqlValue::Text(r#"["scope-one"]"#.into()),
                    SqlValue::Text(actor.into()),
                    SqlValue::Integer(0),
                ],
                label: Some("test.message_threads_list".into()),
            })
            .await
            .expect("message thread scope query");
        let threads: Vec<String> = rows
            .iter()
            .map(|row| match row.get("thread_id") {
                Some(SqlValue::Text(thread)) => thread.clone(),
                other => panic!("thread_id column: {other:?}"),
            })
            .collect();
        assert_eq!(threads, expected, "actor {actor}");
    }
}

#[tokio::test]
async fn message_thread_namespace_json_scope_handles_empty_single_and_past_bind_limit() {
    use khive_runtime::{KhiveRuntime, Namespace};
    use khive_storage::types::{SqlStatement, SqlValue};

    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let namespace = Namespace::parse("scope-one").expect("valid namespace");
    let token = runtime.authorize(namespace).expect("authorized namespace");
    runtime
        .create_note(
            &token,
            "message",
            None,
            "threaded message",
            None,
            Some(serde_json::json!({
                "thread_id": "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"
            })),
            vec![],
        )
        .await
        .expect("create message note");

    let past_bind_limit = (0..33_000)
        .map(|index| format!("scope-{index}"))
        .chain(std::iter::once("scope-one".to_string()))
        .collect::<Vec<_>>();
    for (case, namespaces, expected_rows) in [
        ("empty", Vec::<String>::new(), 0),
        ("single", vec!["scope-one".to_string()], 1),
        ("past SQLite bind limit", past_bind_limit, 1),
    ] {
        let namespaces_json =
            serde_json::to_string(&namespaces).expect("serialize namespace scope");
        let mut reader = runtime.sql().reader().await.expect("SQL reader");
        let rows = reader
            .query_all(SqlStatement {
                sql: sql!("message_threads_list").to_string(),
                params: vec![
                    SqlValue::Text(namespaces_json),
                    SqlValue::Text("local".into()),
                    SqlValue::Integer(1),
                ],
                label: Some("test.message_threads_list".into()),
            })
            .await
            .expect("message thread scope query");
        assert_eq!(rows.len(), expected_rows, "{case} namespace scope");
    }
}

async fn archive_list_fixture() -> (
    khive_runtime::KhiveRuntime,
    khive_runtime::NamespaceToken,
    [uuid::Uuid; 4],
) {
    let runtime = khive_runtime::KhiveRuntime::memory().unwrap();
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .unwrap();
    let mut ids = Vec::new();
    for (index, status) in ["archived", "active", "archived", "active"]
        .into_iter()
        .enumerate()
    {
        let mut note =
            khive_storage::note::Note::new("local", "observation", "archive selector fixture");
        note.status = status.into();
        note.created_at = 40 - index as i64 * 10;
        note.updated_at = note.created_at;
        note.key = Some(format!("archive-probe/{index}"));
        note.properties = Some(
            serde_json::json!({"status": if status == "active" { "archived" } else { "active" }}),
        );
        ids.push(note.id);
        runtime
            .notes(&token)
            .unwrap()
            .upsert_note(note)
            .await
            .unwrap();
    }
    (runtime, token, ids.try_into().unwrap())
}

fn archive_list_rows(response: &serde_json::Value) -> &[serde_json::Value] {
    response
        .get("items")
        .or_else(|| response.get("notes"))
        .unwrap()
        .as_array()
        .unwrap()
}

#[tokio::test]
async fn kg_note_status_defaults_and_explicit_selectors_cover_all_list_routes() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let (runtime, token, ids) = archive_list_fixture().await;
        let mut builder = khive_runtime::VerbRegistryBuilder::new();
        builder.register(crate::KgPack::new(runtime.clone()));
        let registry = builder.build().unwrap();
        let pack = crate::KgPack::new(runtime.clone());
        for mode in [serde_json::json!({}), serde_json::json!({"after": ""}), serde_json::json!({"key_prefix": "archive-probe/"})] {
            for (selector, expected_first, expected_count) in [(None, ids[1], 2), (Some("archived"), ids[0], 2), (Some("any"), ids[0], 4)] {
                let mut params = serde_json::json!({"kind": "note", "limit": 1});
                params.as_object_mut().unwrap().extend(mode.as_object().unwrap().clone());
                if let Some(selector) = selector { params["note_status"] = selector.into(); }
                let result = pack.handle_list(&token, params.clone(), &registry).await.unwrap();
                let rows = archive_list_rows(&result);
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0]["id"], expected_first.to_string(), "default must select the active column before pagination: {params}");
                params["limit"] = 10.into();
                let result = pack.handle_list(&token, params, &registry).await.unwrap();
                assert_eq!(archive_list_rows(&result).len(), expected_count);
            }
        }
        let result = pack.handle_list(&token, serde_json::json!({
            "kind": "note", "key_prefix": "archive-probe/", "after_key": "archive-probe/0", "limit": 1,
        }), &registry).await.unwrap();
        assert_eq!(archive_list_rows(&result)[0]["id"], ids[1].to_string(),
            "an archived after_key stays a valid explicit cursor boundary");
    }).await.expect("bounded actual KG list regression");
}

#[tokio::test]
async fn note_status_is_note_only_even_when_null() {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let runtime = khive_runtime::KhiveRuntime::memory().unwrap();
        let token = runtime.authorize(khive_runtime::Namespace::local()).unwrap();
        let mut builder = khive_runtime::VerbRegistryBuilder::new();
        builder.register(crate::KgPack::new(runtime.clone()));
        let registry = builder.build().unwrap();
        let pack = crate::KgPack::new(runtime);
        for kind in ["entity", "edge", "event", "proposal"] {
            for selector in [serde_json::Value::Null, serde_json::json!("any")] {
                let error = pack.handle_list(&token, serde_json::json!({"kind": kind, "note_status": selector}), &registry).await.unwrap_err();
                assert!(matches!(error, khive_runtime::RuntimeError::InvalidInput(ref message) if message.contains("note_status") && message.contains("note lists")));
            }
        }
    }).await.expect("bounded note-only selector scope regression");
}

#[test]
fn note_status_parser_and_workflow_status_are_distinct() {
    use khive_storage::note::NoteStatusSelector;
    for (wire, expected) in [
        ("active", NoteStatusSelector::Active),
        ("archived", NoteStatusSelector::Archived),
        ("any", NoteStatusSelector::Any),
    ] {
        let params: ListParams =
            serde_json::from_value(serde_json::json!({"kind": "note", "note_status": wire}))
                .unwrap();
        assert_eq!(params.note_status, Some(expected));
    }
    for invalid in [
        serde_json::json!("deleted"),
        serde_json::json!("Active"),
        serde_json::json!("pending"),
        serde_json::json!(1),
    ] {
        assert!(serde_json::from_value::<ListParams>(
            serde_json::json!({"kind": "note", "note_status": invalid})
        )
        .is_err());
    }
    let params: ListParams = serde_json::from_value(
        serde_json::json!({"kind": "note", "note_status": "archived", "status": "pending"}),
    )
    .unwrap();
    let filter = crate::handlers::note_list::note_filter(&params, Some("scheduled_event")).unwrap();
    assert_eq!(filter.note_status, Some(NoteStatusSelector::Archived));
    assert!(filter.property_filters.iter().any(|predicate| predicate.json_path == "$.status"
        && matches!(&predicate.value, khive_storage::SqlValue::Text(value) if value == "pending")));
    let params: ListParams =
        serde_json::from_value(serde_json::json!({"kind": "note", "note_status": null})).unwrap();
    assert_eq!(
        crate::handlers::note_list::note_filter(&params, None)
            .unwrap()
            .note_status,
        Some(NoteStatusSelector::Active)
    );
}

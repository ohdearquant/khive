//! Real producer acceptance through the public MCP request parser and registry.
//! Reader audits are outside the frozen window, so parity has a fixed population.

use std::collections::BTreeMap;

use khive_mcp::{server::KhiveMcpServer, tools::request::RequestParams};
use khive_runtime::{micros_to_iso, KhiveRuntime, Namespace, RuntimeConfig};
use khive_storage::{types::PageRequest, Event, EventFilter};
use khive_types::{EventKind, SubstrateKind};
use serde_json::{json, Value};

const ACTOR: &str = "event-page-writer";
const NAMESPACE: &str = "event-page-acceptance";

fn fixture() -> (KhiveRuntime, KhiveMcpServer) {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        actor_id: Some(ACTOR.into()),
        default_namespace: Namespace::parse(NAMESPACE).unwrap(),
        packs: vec!["kg".into(), "gtd".into(), "brain".into()],
        ..RuntimeConfig::no_embeddings()
    })
    .expect("isolated runtime");
    let server = KhiveMcpServer::new(runtime.clone()).expect("real pack registry");
    (runtime, server)
}

async fn request(server: &KhiveMcpServer, verb: &str, args: Value) -> Value {
    let response = server
        .dispatch_request_local(RequestParams {
            ops: json!([{"tool": verb, "args": args}]).to_string(),
            presentation: Some("verbose".into()),
            format: Some("json".into()),
            ..RequestParams::default()
        })
        .await
        .expect("MCP request dispatch");
    let body: Value = serde_json::from_str(&response).expect("MCP JSON response");
    assert_eq!(body["results"].as_array().unwrap().len(), 1);
    let result = &body["results"][0];
    assert_eq!(result["ok"], true, "{verb}: {body}");
    result["result"].clone()
}

async fn walk(server: &KhiveMcpServer, mut args: Value) -> Vec<Value> {
    let mut rows = Vec::new();
    let mut frozen_until = None;
    let mut previous_cursor = None;
    for page_number in 0..100 {
        let page = request(server, "brain.event_page", args.clone()).await;
        assert_eq!(page["consistency"], "live_ordered_window");
        let events = page["events"].as_array().unwrap();
        assert_eq!(page["count"], events.len());
        match &frozen_until {
            Some(until) => assert_eq!(&page["until"], until),
            None => frozen_until = Some(page["until"].clone()),
        }
        rows.extend(events.iter().cloned());
        if page["has_more"] == false {
            assert!(page["next_after"].is_null());
            assert!(page_number > 0, "fixture must exercise continuation");
            return rows;
        }
        assert!(!events.is_empty(), "a peek cannot advance an empty page");
        let cursor = page["next_after"].as_str().unwrap().to_string();
        assert_ne!(previous_cursor.as_ref(), Some(&cursor));
        previous_cursor = Some(cursor.clone());
        args["after"] = json!(cursor);
        // A continuation may omit the frozen until and change only page size.
        args.as_object_mut().unwrap().remove("until");
        args["limit"] = json!(2);
    }
    panic!("page walk failed to terminate within the bounded fixture");
}

fn by_id(rows: &[Value]) -> BTreeMap<String, Value> {
    let mut result = BTreeMap::new();
    for row in rows {
        let id = row["id"].as_str().expect("canonical event ID");
        assert_eq!(uuid::Uuid::parse_str(id).unwrap().to_string(), id);
        assert!(result.insert(id.to_owned(), row.clone()).is_none());
    }
    result
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn real_feedback_and_task_writers_page_exact_stored_fields_and_count_population() {
    let (runtime, server) = fixture();
    let since_us = chrono::Utc::now().timestamp_micros() - 1_000_000;
    let target = request(
        &server,
        "create",
        json!({"kind": "concept", "name": "Event page feedback target"}),
    )
    .await;
    let feedback = request(
        &server,
        "brain.feedback",
        json!({"target_id": target["id"], "signal": "useful"}),
    )
    .await;
    assert_eq!(feedback["emitted"], true);

    let task = request(
        &server,
        "gtd.assign",
        json!({"title": "Event page task", "status": "next"}),
    )
    .await;
    let task_id = task["full_id"].as_str().expect("canonical task ID");
    let transitioned = request(
        &server,
        "gtd.transition",
        json!({"id": task_id, "status": "active"}),
    )
    .await;
    assert_eq!(transitioned["transitioned"], true);
    assert_eq!(transitioned["from"], "next");
    assert_eq!(transitioned["to"], "active");
    request(&server, "gtd.complete", json!({"id": task_id})).await;

    let token = runtime
        .authorize(Namespace::parse(NAMESPACE).unwrap())
        .unwrap();
    let store = runtime.events(&token).unwrap();
    // A legacy event-plane audit has no task identity/status to reconstruct.
    let mut historical = Event::new(
        NAMESPACE,
        "gtd.transition",
        EventKind::Audit,
        SubstrateKind::Note,
        ACTOR,
    )
    .with_payload(json!({"historical_fixture": true}));
    historical.created_at = since_us + 1;
    let historical_id = historical.id.to_string();
    store.append_event(historical).await.unwrap();

    let until_us = chrono::Utc::now().timestamp_micros();
    let since = micros_to_iso(since_us);
    let until = micros_to_iso(until_us);
    let stored = store
        .query_events(
            EventFilter {
                actors: vec![ACTOR.into(), format!("actor:{ACTOR}")],
                after: Some(since_us - 1),
                before: Some(until_us),
                ..EventFilter::default()
            },
            PageRequest {
                limit: 500,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert!(stored.items.len() > 3 && stored.items.len() < 500);
    let expected: Vec<Value> = stored
        .items
        .iter()
        .map(|event| {
            let mut row = serde_json::to_value(event).unwrap();
            row["created_at"] = json!(micros_to_iso(event.created_at));
            row
        })
        .collect();
    let rows = walk(&server, json!({"since": since, "until": until, "limit": 1})).await;
    let actual_by_id = by_id(&rows);
    assert_eq!(
        actual_by_id,
        by_id(&expected),
        "all persisted fields survive"
    );
    for pair in rows.windows(2) {
        let key = |row: &Value| {
            (
                chrono::DateTime::parse_from_rfc3339(row["created_at"].as_str().unwrap())
                    .unwrap()
                    .timestamp_micros(),
                row["id"].as_str().unwrap().to_string(),
            )
        };
        assert!(key(&pair[0]) < key(&pair[1]));
    }

    let feedback_row = &actual_by_id[feedback["event_id"].as_str().unwrap()];
    assert_eq!(feedback_row["kind"], "feedback_explicit");
    assert_eq!(feedback_row["target_id"], target["id"]);
    assert_eq!(feedback_row["payload"]["signal"], "useful");
    assert_eq!(
        feedback_row["payload"]["served_by_profile_id"],
        feedback["served_by_profile_id"]
    );
    let task_rows: Vec<_> = rows
        .iter()
        .filter(|row| {
            matches!(
                row["verb"].as_str(),
                Some("gtd.transition" | "gtd.complete")
            )
        })
        .collect();
    assert!(
        task_rows.len() >= 3,
        "real transition, completion and legacy row"
    );
    assert!(actual_by_id.contains_key(&historical_id));
    for row in task_rows {
        assert_eq!(row["kind"], "audit");
        assert!(row["target_id"].is_null());
        for field in ["task_id", "prior_status", "new_status", "from", "to"] {
            assert!(
                row["payload"].get(field).is_none(),
                "must not infer {field}: {row}"
            );
        }
    }

    let counts = request(
        &server,
        "brain.event_counts",
        json!({"since": since, "until": until, "exhaustive": true}),
    )
    .await;
    assert_eq!(counts["truncated"], false);
    assert_eq!(counts["window_event_total"], rows.len());
}

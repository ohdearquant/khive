mod common;

use std::collections::BTreeSet;

use common::{assign, pack, rt};
use khive_runtime::{
    present, render_format, KhiveRuntime, Namespace, OutputFormat, PresentationMode,
};
use khive_storage::{types::PageRequest, Note};
use serde_json::{json, Value};
use uuid::Uuid;

async fn seed_tasks(runtime: &KhiveRuntime, count: u32) -> Vec<Uuid> {
    let mut notes: Vec<Note> = (0..count)
        .map(|index| {
            let mut note = Note::new("local", "task", format!("page content {index}"));
            note.id = Uuid::from_u128((u128::from(index) + 1) << 96);
            note.name = Some(format!("page-task-{index:03}"));
            note.properties = Some(json!({
                "status": "next",
                "priority": "p2",
                "assignee": "page-reader",
                "description": format!("stored description {index}"),
            }));
            note.created_at = 1_000_000 + i64::from(index);
            note.updated_at = note.created_at;
            note
        })
        .collect();
    if count >= 3 {
        let blocker = notes[0].id;
        let last = notes.len() - 1;
        notes[last].properties.as_mut().unwrap()["depends_on"] = json!([blocker]);
        notes[last - 1].properties.as_mut().unwrap()["depends_on"] =
            json!([Uuid::from_u128(u128::MAX)]);
    }
    let ids = notes.iter().rev().map(|note| note.id).collect();
    let token = runtime.authorize(Namespace::local()).unwrap();
    runtime
        .notes(&token)
        .unwrap()
        .upsert_notes(notes)
        .await
        .expect("seed task rows");
    ids
}

async fn stored_notes(runtime: &KhiveRuntime) -> Vec<Note> {
    let token = runtime.authorize(Namespace::local()).unwrap();
    let page = runtime
        .notes(&token)
        .unwrap()
        .query_notes(
            "local",
            None,
            PageRequest {
                limit: 1_000,
                offset: 0,
            },
        )
        .await
        .expect("read complete local note fixture");
    assert_eq!(page.total, Some(page.items.len() as u64));
    page.items
}

fn assert_keys(value: &Value, expected: &[&str]) {
    let actual: BTreeSet<_> = value
        .as_object()
        .expect("object response")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(actual, expected.iter().copied().collect());
}

fn assert_ids(rows: &[Value], expected: &[Uuid]) {
    let actual: Vec<_> = rows
        .iter()
        .map(|row| row["full_id"].as_str().expect("full task UUID").to_owned())
        .collect();
    let expected: Vec<_> = expected.iter().map(Uuid::to_string).collect();
    assert_eq!(actual, expected);
}

fn assert_continued(response: &Value, expected: &[Uuid]) {
    assert_keys(response, &["tasks", "has_more"]);
    assert_eq!(response["has_more"], true);
    assert_ids(response["tasks"].as_array().unwrap(), expected);
}

fn assert_complete(response: &Value, expected: &[Uuid]) {
    assert_ids(
        response
            .as_array()
            .expect("an exhausted unclamped page remains an array"),
        expected,
    );
}

#[tokio::test]
async fn tasks_discloses_more_rows_at_default_and_cap_boundaries() {
    let runtime = rt();
    let fixture = pack(runtime.clone());
    let ids = seed_tasks(&runtime, 201).await;
    let before = stored_notes(&runtime).await;
    assert_eq!(before.len(), 201);

    let default = fixture.dispatch("gtd.tasks", json!({})).await.unwrap();
    assert_continued(&default, &ids[..50]);
    assert_eq!(default["tasks"][0]["dependency_state"], "blocked");
    assert_eq!(default["tasks"][0]["actionable"], false);
    assert_eq!(default["tasks"][1]["dependency_state"], "broken");
    assert_eq!(default["tasks"][1]["actionable"], false);

    for limit in [0_u32, 1, 200] {
        let page = fixture
            .dispatch("gtd.tasks", json!({"limit": limit}))
            .await
            .unwrap();
        assert_continued(&page, &ids[..limit.max(1) as usize]);
    }
    let last = fixture
        .dispatch("gtd.tasks", json!({"offset": 200}))
        .await
        .unwrap();
    assert_complete(&last, &ids[200..]);
    let empty = fixture
        .dispatch("gtd.tasks", json!({"offset": 201}))
        .await
        .unwrap();
    assert_eq!(empty, json!([]));

    let full_cap_final = fixture
        .dispatch("gtd.tasks", json!({"limit": 200, "offset": 1}))
        .await
        .unwrap();
    assert_complete(&full_cap_final, &ids[1..]);
    let full_default_final = fixture
        .dispatch("gtd.tasks", json!({"offset": 151}))
        .await
        .unwrap();
    assert_complete(&full_default_final, &ids[151..]);
    assert_eq!(stored_notes(&runtime).await, before);
}

#[tokio::test]
async fn tasks_distinguishes_clamped_limit_from_remaining_matches() {
    let runtime = rt();
    let fixture = pack(runtime.clone());
    let ids = seed_tasks(&runtime, 201).await;
    let terminal = assign(
        &fixture,
        json!({"title": "closed page fixture", "assignee": "closed-reader"}),
    )
    .await;
    fixture
        .dispatch("gtd.complete", json!({"id": terminal["full_id"]}))
        .await
        .unwrap();
    let before = stored_notes(&runtime).await;
    assert_eq!(before.len(), 202);

    let first = fixture
        .dispatch(
            "gtd.tasks",
            json!({"assignee": "page-reader", "limit": 500}),
        )
        .await
        .unwrap();
    assert_keys(
        &first,
        &[
            "tasks",
            "has_more",
            "requested_limit",
            "effective_limit",
            "limit_clamped",
        ],
    );
    assert_ids(first["tasks"].as_array().unwrap(), &ids[..200]);
    assert_eq!(first["has_more"], true);
    assert_eq!(first["requested_limit"], 500);
    assert_eq!(first["effective_limit"], 200);
    assert_eq!(first["limit_clamped"], true);

    for (offset, expected) in [(1, &ids[1..]), (201, &ids[201..])] {
        let complete = fixture
            .dispatch(
                "gtd.tasks",
                json!({"assignee": "page-reader", "limit": 500, "offset": offset}),
            )
            .await
            .unwrap();
        assert_keys(
            &complete,
            &[
                "tasks",
                "requested_limit",
                "effective_limit",
                "limit_clamped",
            ],
        );
        assert_ids(complete["tasks"].as_array().unwrap(), expected);
        assert_eq!(complete["requested_limit"], 500);
        assert_eq!(complete["effective_limit"], 200);
        assert_eq!(complete["limit_clamped"], true);
    }

    let excluded = fixture
        .dispatch("gtd.tasks", json!({"assignee": "closed-reader"}))
        .await
        .unwrap();
    assert_keys(&excluded, &["tasks", "filter_excluded", "hint"]);
    assert_eq!(excluded["tasks"], json!([]));
    assert_eq!(
        excluded["filter_excluded"],
        json!(["done", "cancelled", "unrecognized_status"])
    );
    assert!(excluded["hint"]
        .as_str()
        .unwrap()
        .contains("status=\"done\""));
    let excluded_clamped = fixture
        .dispatch(
            "gtd.tasks",
            json!({"assignee": "closed-reader", "limit": 500}),
        )
        .await
        .unwrap();
    let mut expected = excluded;
    expected["requested_limit"] = json!(500);
    expected["effective_limit"] = json!(200);
    expected["limit_clamped"] = json!(true);
    assert_eq!(excluded_clamped, expected);
    assert_eq!(stored_notes(&runtime).await, before);
}

#[tokio::test]
async fn tasks_continuation_survives_presentation() {
    let runtime = rt();
    let fixture = pack(runtime.clone());
    let ids = seed_tasks(&runtime, 3).await;
    let before = stored_notes(&runtime).await;
    let response = fixture
        .dispatch("gtd.tasks", json!({"limit": 2}))
        .await
        .unwrap();
    assert_continued(&response, &ids[..2]);

    for mode in [PresentationMode::Agent, PresentationMode::Verbose] {
        let presented = present(response.clone(), mode, 0);
        let text = render_format(presented.clone(), OutputFormat::Json, mode);
        let decoded: Value = serde_json::from_str(&text).unwrap();
        assert_continued(&decoded, &ids[..2]);

        for format in [OutputFormat::Auto, OutputFormat::Table] {
            let table = render_format(presented.clone(), format, mode);
            let lines: Vec<_> = table.lines().collect();
            assert_eq!(lines.len(), 5, "{mode:?} {format:?}: {table}");
            assert!(lines[0].starts_with('|'), "actual table: {table}");
            assert!(lines[1].starts_with("|---|"), "table separator: {table}");
            assert_eq!(lines[4], "has_more: true");
            for (row, index) in lines[2..4].iter().zip([2, 1]) {
                assert!(row.contains(&format!(" page-task-{index:03} |")));
                assert!(row.contains(&format!(" {:08x} |", index + 1)));
            }
            assert!(!table.contains("page-task-000"));
        }
    }
    assert_eq!(stored_notes(&runtime).await, before);
}

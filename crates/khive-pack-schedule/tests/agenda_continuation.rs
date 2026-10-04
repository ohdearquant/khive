//! Dispatch regressions for agenda's exclusive continuation (#3823).

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use khive_pack_schedule::SchedulePack;
use khive_runtime::{KhiveRuntime, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use serde_json::{json, Value};
use uuid::Uuid;

mod support;

const TIED_AT: &str = "2099-01-01T10:00:00Z";

fn build_registry() -> (VerbRegistry, KhiveRuntime) {
    let runtime = support::memory_runtime();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(khive_pack_comm::CommPack::new(runtime.clone()));
    builder.register(SchedulePack::new(runtime.clone()));
    (builder.build().expect("registry builds"), runtime)
}

#[derive(Clone)]
struct SeededEvent {
    id: String,
    at: String,
}

async fn remind(registry: &VerbRegistry, content: &str, at: &str) -> SeededEvent {
    let result = registry
        .dispatch("schedule.remind", json!({ "content": content, "at": at }))
        .await
        .expect("fixture reminder created");
    SeededEvent {
        id: result["full_id"].as_str().expect("full reminder ID").into(),
        at: at.into(),
    }
}

fn expected_ids(mut seeded: Vec<SeededEvent>) -> Vec<String> {
    // Derive the oracle from the fixture inputs, independently of an agenda read.
    seeded.sort_by_key(|event| {
        (
            event.at.parse::<DateTime<Utc>>().expect("fixture instant"),
            event.at.clone(),
            event.id.clone(),
        )
    });
    seeded.into_iter().map(|event| event.id).collect()
}

fn event_ids(page: &Value) -> Vec<String> {
    let events = page["events"].as_array().expect("agenda events array");
    assert_eq!(page["count"].as_u64(), Some(events.len() as u64));
    events
        .iter()
        .map(|event| event["full_id"].as_str().expect("event full ID").into())
        .collect()
}

fn assert_last_cursor(page: &Value) {
    let events = page["events"].as_array().expect("agenda events array");
    let next = page.get("next").expect("agenda includes next");
    match events.last() {
        None => assert!(next.is_null(), "empty page must have next=null: {page}"),
        Some(last) => {
            assert_eq!(
                next,
                &json!({
                    "after": last["properties"]["trigger_at"],
                    "after_id": last["full_id"],
                }),
                "next must name the last row's verbatim timestamp text and full UUID"
            );
            let id = next["after_id"].as_str().expect("cursor full ID");
            assert_eq!(id.len(), 36, "returned cursor ID must be full UUID text");
            assert_eq!(Uuid::parse_str(id).unwrap().to_string(), id);
        }
    }
}

async fn walk_agenda(registry: &VerbRegistry, mut args: Value, expected: &[String]) -> Vec<Value> {
    let mut seen = BTreeSet::new();
    let mut events = Vec::new();
    // Every nonempty page must contribute a new ID. An ignored/rewound cursor
    // fails here, and an extra page also cannot make this loop run forever.
    for _ in 0..=expected.len() {
        let page = registry
            .dispatch("schedule.agenda", args.clone())
            .await
            .expect("continued agenda succeeds");
        let ids = event_ids(&page);
        assert_last_cursor(&page);
        for id in &ids {
            assert!(seen.insert(id.clone()), "continuation repeated ID {id}");
        }
        events.extend(page["events"].as_array().unwrap().iter().cloned());
        if ids.is_empty() {
            let actual: Vec<_> = events
                .iter()
                .map(|event| event["full_id"].as_str().unwrap().to_string())
                .collect();
            assert_eq!(
                actual, expected,
                "pagination must return each fixture ID in order"
            );
            return events;
        }
        args["after"] = page["next"]["after"].clone();
        args["after_id"] = page["next"]["after_id"].clone();
    }
    panic!("agenda continuation did not terminate within the fixture bound");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn continuation_pages_every_tied_event_once_and_preserves_from_only_pages() {
    let (registry, _runtime) = build_registry();
    let mut seeded = Vec::new();
    for index in 0..17 {
        seeded.push(remind(&registry, &format!("tie-{index}"), TIED_AT).await);
    }
    let expected = expected_ids(seeded);
    let legacy = registry
        .dispatch("schedule.agenda", json!({ "from": TIED_AT, "limit": 200 }))
        .await
        .unwrap();
    assert_eq!(
        event_ids(&legacy),
        expected,
        "legacy from remains inclusive"
    );
    assert_last_cursor(&legacy);

    let first = registry
        .dispatch("schedule.agenda", json!({ "from": TIED_AT, "limit": 4 }))
        .await
        .unwrap();
    assert_eq!(event_ids(&first), expected[..4]);
    assert_eq!(
        first["events"].as_array().unwrap().as_slice(),
        &legacy["events"].as_array().unwrap()[..4],
        "the existing from-only page retains its exact event projection"
    );
    assert_last_cursor(&first);
    let walked = walk_agenda(&registry, json!({ "from": TIED_AT, "limit": 4 }), &expected).await;
    assert_eq!(walked, *legacy["events"].as_array().unwrap());

    let legacy_again = registry
        .dispatch("schedule.agenda", json!({ "from": TIED_AT, "limit": 4 }))
        .await
        .unwrap();
    assert_eq!(
        legacy_again, first,
        "cursor calls must not move legacy paging state"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn continuation_crosses_the_internal_64_row_page_and_names_the_70th_row() {
    let (registry, _runtime) = build_registry();
    let mut seeded = Vec::new();
    for index in 0..73 {
        seeded.push(remind(&registry, &format!("internal-page-{index}"), TIED_AT).await);
    }
    let expected = expected_ids(seeded);
    let first = registry
        .dispatch("schedule.agenda", json!({ "from": TIED_AT, "limit": 70 }))
        .await
        .unwrap();
    assert_eq!(event_ids(&first), expected[..70]);
    assert_last_cursor(&first);
    assert_eq!(first["next"]["after_id"], expected[69]);

    let last = registry
        .dispatch(
            "schedule.agenda",
            json!({
                "from": TIED_AT, "limit": 70,
                "after": first["next"]["after"],
                "after_id": first["next"]["after_id"],
            }),
        )
        .await
        .unwrap();
    assert_eq!(event_ids(&last), expected[70..]);
    assert_last_cursor(&last);
    assert_eq!(last["next"]["after_id"], expected[72]);
    let empty = registry
        .dispatch(
            "schedule.agenda",
            json!({
                "from": TIED_AT, "limit": 70,
                "after": last["next"]["after"],
                "after_id": last["next"]["after_id"],
            }),
        )
        .await
        .unwrap();
    assert!(event_ids(&empty).is_empty());
    assert_last_cursor(&empty);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn continuation_keeps_timezone_text_ties_and_the_inclusive_window_intersection() {
    let (registry, runtime) = build_registry();
    remind(&registry, "before-window", "2099-01-01T09:59:59Z").await;
    let mut included = Vec::new();
    // Insert out of UTC/raw-text order, including two identical-text ties.
    for (index, at) in [
        "2099-01-02T00:00:00+14:00",
        TIED_AT,
        "2099-01-01T05:00:00-05:00",
        "2098-12-31T23:00:00-11:00",
        TIED_AT,
        "2099-01-01T05:00:01-05:00",
    ]
    .into_iter()
    .enumerate()
    {
        included.push(remind(&registry, &format!("window-{index}"), at).await);
    }
    remind(&registry, "after-window", "2099-01-01T10:00:02Z").await;
    let cancelled = remind(&registry, "cancelled-in-window", TIED_AT).await;
    registry
        .dispatch("schedule.cancel", json!({ "id": cancelled.id }))
        .await
        .unwrap();

    // A matching row in another namespace must not enter either cursor page.
    let token = runtime.authorize(khive_types::Namespace::local()).unwrap();
    let store = runtime.notes(&token).unwrap();
    let mut foreign = store
        .get_note(Uuid::parse_str(&included[0].id).unwrap())
        .await
        .unwrap()
        .unwrap();
    foreign.id = Uuid::new_v4();
    foreign.namespace = "agenda-foreign".into();
    foreign.content = "foreign-in-window".into();
    store.upsert_note(foreign).await.unwrap();

    let expected = expected_ids(included);
    let window = json!({
        "from": "2099-01-01T05:00:00-05:00",
        "to": "2099-01-01T10:00:01Z",
        "limit": 200,
    });
    let legacy = registry
        .dispatch("schedule.agenda", window.clone())
        .await
        .unwrap();
    assert_eq!(
        event_ids(&legacy),
        expected,
        "both inclusive window boundaries remain"
    );
    assert_last_cursor(&legacy);

    let walked = walk_agenda(
        &registry,
        json!({
            "from": window["from"], "to": window["to"], "limit": 1,
            // This key intentionally names no row. The inclusive from bound
            // still excludes before-window; the continuation does not replace it.
            "after": "2099-01-01T09:59:00Z", "after_id": Uuid::nil().to_string(),
        }),
        &expected,
    )
    .await;
    assert_eq!(walked, *legacy["events"].as_array().unwrap());
    let empty = registry
        .dispatch(
            "schedule.agenda",
            json!({
                "from": window["from"], "to": window["to"], "limit": 1,
                "after": "2099-01-01T10:00:02Z", "after_id": Uuid::nil().to_string(),
            }),
        )
        .await
        .unwrap();
    assert!(
        event_ids(&empty).is_empty(),
        "cursor and window are an intersection"
    );
    assert_last_cursor(&empty);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn continuation_rejects_partial_or_invalid_keys_without_changing_scheduled_rows() {
    let (registry, runtime) = build_registry();
    let seeded = remind(&registry, "validation-survivor", TIED_AT).await;
    let token = runtime.authorize(khive_types::Namespace::local()).unwrap();
    let store = runtime.notes(&token).unwrap();
    let id = Uuid::parse_str(&seeded.id).unwrap();
    let before = serde_json::to_value(store.get_note(id).await.unwrap().unwrap()).unwrap();
    let legacy = registry
        .dispatch("schedule.agenda", json!({ "from": TIED_AT, "limit": 10 }))
        .await
        .unwrap();
    assert_eq!(event_ids(&legacy), vec![seeded.id.clone()]);

    for (label, args) in [
        ("missing UUID", json!({ "after": TIED_AT })),
        ("missing timestamp", json!({ "after_id": seeded.id })),
        (
            "invalid timestamp",
            json!({ "after": "not-a-date", "after_id": seeded.id }),
        ),
        (
            "invalid UUID",
            json!({ "after": TIED_AT, "after_id": "not-a-uuid" }),
        ),
        (
            "UUID prefix",
            json!({ "after": TIED_AT, "after_id": &seeded.id[..8] }),
        ),
    ] {
        let error = registry
            .dispatch("schedule.agenda", args)
            .await
            .expect_err(label);
        assert!(
            matches!(&error, RuntimeError::InvalidInput(_)),
            "{label}: {error}"
        );
        assert!(error.to_string().contains("after"), "{label}: {error}");
        let after = serde_json::to_value(store.get_note(id).await.unwrap().unwrap()).unwrap();
        assert_eq!(after, before, "{label} must not mutate the scheduled row");
        let unchanged = registry
            .dispatch("schedule.agenda", json!({ "from": TIED_AT, "limit": 10 }))
            .await
            .unwrap();
        assert_eq!(
            unchanged, legacy,
            "{label} must not alter the agenda population"
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn continuation_response_has_the_declared_key_set() {
    let (registry, _runtime) = build_registry();
    remind(&registry, "response-contract", TIED_AT).await;
    let page = registry
        .dispatch("schedule.agenda", json!({ "from": TIED_AT, "limit": 1 }))
        .await
        .unwrap();
    assert_eq!(
        event_ids(&page).len(),
        1,
        "fixture must produce a nonempty page"
    );
    let keys: BTreeSet<_> = page
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        BTreeSet::from(["events", "count", "next"]),
        "agenda response must have exactly the declared top-level keys"
    );
    let next_keys: BTreeSet<_> = page["next"]
        .as_object()
        .expect("nonempty page has an object continuation")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        next_keys,
        BTreeSet::from(["after", "after_id"]),
        "a nonempty continuation must have exactly its declared pair of keys"
    );
    assert_last_cursor(&page);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn continuation_empty_page_returns_null() {
    let (registry, _runtime) = build_registry();
    let empty = registry
        .dispatch("schedule.agenda", json!({ "from": TIED_AT, "limit": 1 }))
        .await
        .unwrap();
    assert_eq!(empty, json!({ "events": [], "count": 0, "next": null }));

    let seeded = remind(&registry, "last-position", TIED_AT).await;
    let last = registry
        .dispatch("schedule.agenda", json!({ "from": TIED_AT, "limit": 1 }))
        .await
        .unwrap();
    assert_eq!(event_ids(&last), vec![seeded.id]);
    assert_last_cursor(&last);
    let after_last = registry
        .dispatch(
            "schedule.agenda",
            json!({
                "from": TIED_AT, "limit": 1,
                "after": last["next"]["after"],
                "after_id": last["next"]["after_id"],
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        after_last,
        json!({ "events": [], "count": 0, "next": null }),
        "an exhausted continuation must return an explicit null with zero rows"
    );
}

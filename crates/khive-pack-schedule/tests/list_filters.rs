//! Public list filtering over scheduled-event metadata, before every page boundary.

mod support;

use khive_pack_kg::KgPack;
use khive_pack_schedule::SchedulePack;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use khive_storage::Note;
use serde_json::{json, Value};

const OWNER: &str = "lambda:owner";
const BASE_TIME: i64 = 1_800_000_000_000_000;

fn registry(runtime: &KhiveRuntime, visible: &[&str]) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("lambda:reader".into()));
    builder.with_visible_namespaces(
        visible
            .iter()
            .map(|name| Namespace::parse(name).unwrap())
            .collect(),
    );
    builder.register(KgPack::new(runtime.clone()));
    builder.register(SchedulePack::new(runtime.clone()));
    builder.build().unwrap()
}

fn scheduled(namespace: &str, status: &str, creator: &str, key: Option<String>, time: i64) -> Note {
    let mut note = Note::new(namespace, "scheduled_event", "scheduled intent fixture");
    note.key = key;
    note.created_at = BASE_TIME + time;
    note.updated_at = BASE_TIME + time;
    note.properties = Some(json!({"status": status, "created_by_actor": creator,
        "tags": ["job", "blue"]}));
    note
}

async fn seed(runtime: &KhiveRuntime, notes: Vec<Note>) {
    // Historical states and keys are fixtures below the schedule-managed write
    // boundary. Every assertion exercises the public list dispatcher.
    let token = runtime.authorize(Namespace::local()).unwrap();
    let summary = runtime
        .notes(&token)
        .unwrap()
        .upsert_notes(notes)
        .await
        .unwrap();
    assert_eq!(summary.failed, 0, "{summary:?}");
    assert_eq!(summary.affected, summary.attempted);
}

fn ids(page: &Value, field: &str) -> Vec<String> {
    page[field]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap().to_owned())
        .collect()
}

fn note_ids(notes: &[Note]) -> Vec<String> {
    notes.iter().map(|note| note.id.to_string()).collect()
}

async fn cursor_walk(registry: &VerbRegistry, mut args: Value) -> Vec<String> {
    args["after"] = json!("");
    args["limit"] = json!(1);
    let mut seen = Vec::new();
    for _ in 0..10 {
        let page = registry.dispatch("list", args.clone()).await.unwrap();
        let rows = ids(&page, "notes");
        assert_eq!(rows.len(), 1, "every filtered page should be full: {page}");
        assert!(page.get("scan_incomplete").is_none(), "{page}");
        assert_eq!(page["has_more"], !page["next_after"].is_null());
        assert!(!seen.contains(&rows[0]), "cursor repeated a matching row");
        seen.extend(rows);
        if page["next_after"].is_null() {
            return seen;
        }
        args["after"] = page["next_after"].clone();
    }
    panic!("filtered cursor did not terminate");
}

async fn pagination_fixture() -> (VerbRegistry, Vec<Note>, Note) {
    let runtime = support::memory_runtime();
    let registry = registry(&runtime, &[]);
    let mut notes = Vec::new();
    // More than one raw page precedes the targets in insertion order, created
    // order, and keyed updated order. A post-page filter would lose them.
    for index in 0..205 {
        notes.push(scheduled(
            "local",
            "pending",
            "lambda:other",
            Some(format!("sched%_/before/{index}")),
            10_000 + index,
        ));
    }
    let targets: Vec<_> = (0..3)
        .map(|index| {
            scheduled(
                "local",
                "missed",
                OWNER,
                Some(format!("sched%_/target/{index}")),
                1_000 + index * 10,
            )
        })
        .collect();
    notes.extend(targets.clone());
    for index in 0..205 {
        notes.push(scheduled(
            "local",
            "pending",
            "lambda:other",
            Some(format!("sched%_/after/{index}")),
            20_000 + index,
        ));
    }
    let boundary = scheduled(
        "local",
        "pending",
        "lambda:other",
        Some("outside-prefix-boundary".into()),
        1_015,
    );
    notes.push(boundary.clone());
    // SQL LIKE would wrongly include this key; prefix matching must stay literal.
    notes.push(scheduled(
        "local",
        "missed",
        OWNER,
        Some("schedXY/decoy".into()),
        999,
    ));
    let mut deleted = scheduled(
        "local",
        "missed",
        OWNER,
        Some("sched%_/deleted".into()),
        1_050,
    );
    deleted.deleted_at = Some(BASE_TIME + 1_060);
    notes.push(deleted);
    seed(&runtime, notes).await;
    (registry, targets, boundary)
}

#[tokio::test]
async fn scheduled_filters_precede_offset_and_insertion_cursor_pagination() {
    let (registry, targets, _) = pagination_fixture().await;
    // The timestamp excludes the fourth metadata match used as a keyed decoy.
    let args = json!({"kind": "scheduled_event", "status": "missed",
        "created_by_actor": OWNER, "created_after":
        chrono::DateTime::from_timestamp_micros(BASE_TIME + 1_000).unwrap().to_rfc3339()});
    let mut expected = note_ids(&targets);
    assert_eq!(cursor_walk(&registry, args.clone()).await, expected);
    expected.reverse();
    for (offset, id) in expected.iter().enumerate() {
        let mut page_args = args.clone();
        page_args["offset"] = json!(offset);
        page_args["limit"] = json!(1);
        let page = registry.dispatch("list", page_args).await.unwrap();
        assert_eq!(ids(&page, "items"), vec![id.clone()]);
        assert_eq!(page["has_more"], offset + 1 < expected.len());
        assert_eq!(page["effective_limit"], 1);
        assert!(page.get("scan_incomplete").is_none());
    }
    // Tags exercise the existing residual-filter scan; the schedule predicates
    // must already constrain its SQL pages. Both kind spellings are supported.
    let mut combined = args.clone();
    combined["kind"] = json!("note");
    combined["note_kind"] = json!("scheduled_event");
    combined["tags"] = json!(["JOB", "blue"]);
    combined["tag_mode"] = json!("all");
    combined["updated_after"] = args["created_after"].clone();
    assert_eq!(
        cursor_walk(&registry, combined.clone()).await,
        note_ids(&targets)
    );
    combined["offset"] = json!(1);
    combined["limit"] = json!(1);
    let page = registry.dispatch("list", combined).await.unwrap();
    assert_eq!(ids(&page, "items"), vec![targets[1].id.to_string()]);
    assert_eq!(page["has_more"], true);
}

#[tokio::test]
async fn scheduled_filters_compose_with_keyed_offsets_cursors_and_after_key() {
    let (registry, targets, boundary) = pagination_fixture().await;
    let args = json!({"kind": "scheduled_event", "status": "missed",
        "created_by_actor": OWNER, "key_prefix": "sched%_"});
    let mut expected = note_ids(&targets);
    expected.reverse();
    assert_eq!(cursor_walk(&registry, args.clone()).await, expected);
    for (offset, id) in expected.iter().enumerate() {
        let mut page_args = args.clone();
        page_args["offset"] = json!(offset);
        page_args["limit"] = json!(1);
        let page = registry.dispatch("list", page_args).await.unwrap();
        assert_eq!(ids(&page, "items"), vec![id.clone()]);
        assert_eq!(page["has_more"], offset + 1 < expected.len());
    }
    let mut after_key = args;
    after_key["after_key"] = json!(boundary.key);
    let page = registry.dispatch("list", after_key).await.unwrap();
    assert_eq!(
        ids(&page, "notes"),
        vec![targets[1].id.to_string(), targets[0].id.to_string()]
    );
    assert_eq!(page["has_more"], false);
    assert!(page["next_after"].is_null());
}

#[tokio::test]
async fn scheduled_filters_preserve_visible_and_primary_namespace_scopes() {
    let runtime = support::memory_runtime();
    let registry = registry(&runtime, &["visible"]);
    let local = scheduled("local", "missed", OWNER, Some("tick/local".into()), 1);
    let visible = scheduled("visible", "missed", OWNER, Some("tick/visible".into()), 2);
    let hidden = scheduled("hidden", "missed", OWNER, Some("tick/hidden".into()), 3);
    seed(&runtime, vec![local.clone(), visible.clone(), hidden]).await;
    // Caller is lambda:reader and these fixtures have no provenance. This is
    // deliberately an other-creator metadata query, never an ownership check.
    let args = json!({"kind": "scheduled_event", "status": "missed", "created_by_actor": OWNER});
    let page = registry.dispatch("list", args.clone()).await.unwrap();
    assert_eq!(
        ids(&page, "items"),
        vec![visible.id.to_string(), local.id.to_string()]
    );
    assert_eq!(
        cursor_walk(&registry, args.clone()).await,
        vec![local.id.to_string(), visible.id.to_string()]
    );
    let mut keyed = args.clone();
    keyed["key_prefix"] = json!("tick/");
    assert_eq!(
        cursor_walk(&registry, keyed.clone()).await,
        vec![local.id.to_string()]
    );
    keyed["offset"] = json!(0);
    assert_eq!(
        ids(&registry.dispatch("list", keyed).await.unwrap(), "items"),
        vec![local.id.to_string()]
    );
    let mut exact_namespace = args;
    exact_namespace["namespace"] = json!("visible");
    assert_eq!(
        ids(
            &registry.dispatch("list", exact_namespace).await.unwrap(),
            "items"
        ),
        vec![visible.id.to_string()]
    );
}

#[tokio::test]
async fn scheduled_filters_match_each_state_and_only_exact_text_metadata() {
    let runtime = support::memory_runtime();
    let registry = registry(&runtime, &[]);
    let states = [
        "provisioning",
        "pending",
        "firing",
        "fired",
        "cancelled",
        "missed",
        "failed",
    ];
    let mut notes: Vec<_> = states
        .iter()
        .enumerate()
        .map(|(index, status)| scheduled("local", status, OWNER, None, index as i64))
        .collect();
    let expected = notes.clone();
    let other = scheduled("local", "missed", "lambda:other", None, 10);
    notes.push(other.clone());
    for value in [
        Value::Null,
        json!(17),
        json!(["lambda:owner"]),
        json!({"id": OWNER}),
    ] {
        let mut malformed = scheduled("local", "missed", OWNER, None, 11);
        malformed.properties.as_mut().unwrap()["created_by_actor"] = value.clone();
        notes.push(malformed);
        let mut malformed = scheduled("local", "missed", OWNER, None, 12);
        malformed.properties.as_mut().unwrap()["status"] = value;
        notes.push(malformed);
    }
    let mut missing = scheduled("local", "missed", OWNER, None, 13);
    missing.properties = Some(json!({}));
    notes.push(missing);
    let text_array = scheduled("local", "missed", "[\"lambda:owner\"]", None, 14);
    notes.push(text_array.clone());
    let mut ordinary = scheduled("local", "missed", OWNER, None, 15);
    ordinary.kind = "observation".into();
    notes.push(ordinary.clone());
    seed(&runtime, notes).await;
    for (state, note) in states.into_iter().zip(&expected) {
        let page = registry
            .dispatch(
                "list",
                json!({"kind":"scheduled_event",
            "status":state, "created_by_actor":OWNER}),
            )
            .await
            .unwrap();
        assert_eq!(ids(&page, "items"), vec![note.id.to_string()]);
    }
    let owner_only = registry
        .dispatch(
            "list",
            json!({"kind":"scheduled_event",
        "created_by_actor":"lambda:other"}),
        )
        .await
        .unwrap();
    assert_eq!(ids(&owner_only, "items"), vec![other.id.to_string()]);
    let state_only = registry
        .dispatch(
            "list",
            json!({"kind":"scheduled_event",
        "status":"firing"}),
        )
        .await
        .unwrap();
    assert_eq!(ids(&state_only, "items"), vec![expected[2].id.to_string()]);
    let page = registry
        .dispatch(
            "list",
            json!({"kind":"scheduled_event",
        "created_by_actor":"[\"lambda:owner\"]"}),
        )
        .await
        .unwrap();
    assert_eq!(ids(&page, "items"), vec![text_array.id.to_string()]);
    for creator in ["LAMBDA:OWNER", "lambda:own", "*", " lambda:owner ", "17"] {
        let page = registry
            .dispatch(
                "list",
                json!({"kind":"scheduled_event",
            "created_by_actor":creator}),
            )
            .await
            .unwrap();
        assert!(ids(&page, "items").is_empty(), "{creator}: {page}");
    }
    let baseline = registry
        .dispatch("list", json!({"kind":"scheduled_event", "limit":200}))
        .await
        .unwrap();
    let null_filters = registry
        .dispatch(
            "list",
            json!({"kind":"scheduled_event",
        "status":null, "created_by_actor":null, "limit":200}),
        )
        .await
        .unwrap();
    assert_eq!(baseline, null_filters);
    assert_eq!(
        ids(
            &registry
                .dispatch("list", json!({"kind":"observation"}))
                .await
                .unwrap(),
            "items"
        ),
        vec![ordinary.id.to_string()]
    );
}

#[tokio::test]
async fn scheduled_filters_accept_publicly_created_schedule_metadata() {
    let runtime = support::memory_runtime();
    let registry = registry(&runtime, &[]);
    let created = registry
        .dispatch(
            "schedule.schedule",
            json!({
        "action":"create(kind=\"concept\", name=\"future concept\")",
        "at":"2099-06-01T10:00:00Z"}),
        )
        .await
        .unwrap();
    let page = registry
        .dispatch(
            "list",
            json!({"kind":"scheduled_event",
        "status":"pending", "created_by_actor":"lambda:reader"}),
        )
        .await
        .unwrap();
    assert_eq!(
        ids(&page, "items"),
        vec![created["full_id"].as_str().unwrap().to_owned()]
    );
}

#[tokio::test]
async fn scheduled_filter_validation_preserves_proposal_status_and_catalog() {
    let runtime = support::memory_runtime();
    let registry = registry(&runtime, &[]);
    for field in ["status", "created_by_actor"] {
        let good = if field == "status" { "missed" } else { OWNER };
        for kind in ["entity", "edge", "event", "observation", "note"] {
            for value in [json!(good), Value::Null] {
                let mut args = json!({"kind":kind});
                args[field] = value;
                let error = registry.dispatch("list", args).await.unwrap_err();
                assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error}");
                assert!(error.to_string().contains("scheduled_event"), "{error}");
            }
        }
        for value in [
            json!(""),
            json!(" \t"),
            json!(true),
            json!(17),
            json!([]),
            json!({}),
        ] {
            let mut args = json!({"kind":"scheduled_event"});
            args[field] = value;
            assert!(matches!(
                registry.dispatch("list", args).await.unwrap_err(),
                RuntimeError::InvalidInput(_)
            ));
        }
    }
    for status in ["bogus", "MISSED", "claimed", "indeterminate", "active"] {
        assert!(registry
            .dispatch("list", json!({"kind":"scheduled_event", "status":status}))
            .await
            .is_err());
    }
    for args in [
        json!({"kind":"note", "note_kind":"observation", "status":"missed"}),
        json!({"kind":"scheduled_event", "note_kind":"observation", "status":"missed"}),
        json!({"kind":"scheduled_event", "status":"missed", "unknown_filter":true}),
        json!({"kind":"proposal", "created_by_actor":OWNER}),
        json!({"kind":"proposal", "created_by_actor":null}),
    ] {
        assert!(registry.dispatch("list", args).await.is_err());
    }
    // "open" is intentionally outside the schedule status vocabulary.
    let proposals = registry
        .dispatch("list", json!({"kind":"proposal", "status":"open"}))
        .await
        .unwrap();
    assert!(ids(&proposals, "items").is_empty());
    let mut kg_only = VerbRegistryBuilder::new();
    kg_only.register(KgPack::new(runtime));
    assert!(kg_only
        .build()
        .unwrap()
        .dispatch("list", json!({"kind":"scheduled_event", "status":"missed"}))
        .await
        .is_err());
    let handlers = registry.all_verbs();
    let list = handlers
        .iter()
        .find(|handler| handler.name == "list")
        .unwrap();
    for name in ["status", "created_by_actor"] {
        let param = list.params.iter().find(|param| param.name == name).unwrap();
        assert!(!param.required);
        assert_eq!(param.param_type, "string");
        assert!(param
            .description
            .to_ascii_lowercase()
            .contains("scheduled_event"));
    }
}

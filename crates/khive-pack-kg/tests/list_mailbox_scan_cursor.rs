//! A cursor returned by `list` must never name a message the caller's mailbox
//! view hides, even when an incomplete scan stops inside a hidden region.

use std::collections::HashSet;

use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, MailboxView, Namespace, VerbRegistry, VerbRegistryBuilder};
use khive_storage::note::{NoteFilter, NoteMailboxScope};
use khive_storage::Note;
use serde_json::{json, Value};

// One more than the per-call scan ceiling in the list handlers.
const HIDDEN: usize = 10_001;

fn registry(rt: &KhiveRuntime) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.build().expect("registry builds")
}

fn hidden_message(i: usize, time: i64, keyed: bool) -> Note {
    let mut note = Note::new("local", "message", format!("hidden {i}")).with_properties(
        json!({"direction": "inbound", "to_actor": "someone-else", "from_actor": "sender"}),
    );
    note.created_at = time;
    note.updated_at = time;
    if keyed {
        note.key = Some(format!("hidden/{i:05}"));
    }
    note
}

fn visible_message(time: i64, keyed: bool) -> Note {
    let mut note = Note::new("local", "message", "visible").with_properties(
        json!({"direction": "inbound", "to_actor": "local", "from_actor": "sender"}),
    );
    note.created_at = time;
    note.updated_at = time;
    if keyed {
        note.key = Some("visible/00000".to_string());
    }
    note
}

async fn seed(rt: &KhiveRuntime, keyed: bool) -> (HashSet<String>, String) {
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    let store = rt.notes(&token).expect("note store");
    // Keyed order is updated_at DESC, so the visible row is the oldest and
    // sorts after every hidden row; insertion order puts it last as well.
    let base = 1_700_000_000_000_000i64;
    let mut hidden_ids = HashSet::new();
    let mut batch = Vec::with_capacity(1000);
    for i in 0..HIDDEN {
        let note = hidden_message(i, base, keyed);
        hidden_ids.insert(note.id.to_string());
        batch.push(note);
        if batch.len() == 1000 {
            store
                .upsert_notes(std::mem::take(&mut batch))
                .await
                .expect("upsert hidden batch");
        }
    }
    store.upsert_notes(batch).await.expect("upsert hidden tail");
    let visible = visible_message(base - 1, keyed);
    let visible_id = visible.id.to_string();
    store.upsert_note(visible).await.expect("upsert visible");
    (hidden_ids, visible_id)
}

/// Follow `next_after` until it is null. Every returned row id and cursor is
/// checked against the hidden set, and the walk must terminate.
async fn walk(
    registry: &VerbRegistry,
    mut args: Value,
    hidden: &HashSet<String>,
    cursor_ids: impl Fn(&str) -> Vec<String>,
) -> Vec<String> {
    args["after"] = json!("");
    args["limit"] = json!(5);
    let mut seen = Vec::new();
    for _ in 0..20 {
        let page = registry
            .dispatch("list", args.clone())
            .await
            .expect("list page");
        for row in page["notes"].as_array().expect("notes array") {
            let id = row["id"].as_str().expect("row id").to_string();
            assert!(!hidden.contains(&id), "hidden row returned: {id}");
            seen.push(id);
        }
        let Some(cursor) = page["next_after"].as_str() else {
            return seen;
        };
        for id in cursor_ids(cursor) {
            assert!(
                !hidden.contains(&id),
                "cursor names a hidden message: {cursor}"
            );
        }
        args["after"] = json!(cursor);
    }
    panic!("continuation did not reach the end within 20 pages");
}

#[tokio::test]
async fn message_list_cursor_never_names_a_hidden_message() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let (hidden, visible_id) = seed(&rt, false).await;
    let reg = registry(&rt);
    let seen = walk(&reg, json!({"kind": "note"}), &hidden, |cursor| {
        vec![cursor.to_string()]
    })
    .await;
    assert_eq!(seen, vec![visible_id]);
}

#[tokio::test]
async fn keyed_list_cursor_never_names_a_hidden_message() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let (hidden, visible_id) = seed(&rt, true).await;
    let reg = registry(&rt);
    let seen = walk(
        &reg,
        json!({"kind": "note", "key_prefix": ""}),
        &hidden,
        |cursor| {
            let body: Value =
                serde_json::from_str(cursor.strip_prefix("nk1:").expect("nk1 cursor"))
                    .expect("cursor json");
            let key = body["key"].as_str().expect("cursor key");
            assert!(
                !key.starts_with("hidden/"),
                "cursor carries a hidden key: {cursor}"
            );
            vec![body["id"].as_str().expect("cursor id").to_string()]
        },
    )
    .await;
    assert_eq!(seen, vec![visible_id]);
}

/// The store-side partition must agree with the row-level rule for every
/// routing shape, so a scan window and the returned page cannot diverge.
#[tokio::test]
async fn store_mailbox_scope_matches_row_level_rule() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    let store = rt.notes(&token).expect("note store");
    let shapes: Vec<Option<Value>> = vec![
        None,
        Some(json!({})),
        Some(json!({"direction": "inbound", "to_actor": "local"})),
        Some(json!({"direction": "inbound", "to_actor": "a"})),
        Some(json!({"direction": "inbound", "to_actor": null})),
        Some(json!({"direction": "inbound"})),
        Some(json!({"direction": "inbound", "to_actor": 5})),
        Some(json!({"direction": "outbound", "from_actor": "local"})),
        Some(json!({"direction": "outbound", "from_actor": "a"})),
        Some(json!({"direction": "outbound", "from_actor": null})),
        Some(json!({"direction": "outbound"})),
        Some(json!({"direction": null})),
        Some(json!({"direction": null, "from_actor": "x"})),
        Some(json!({"to_actor": "local"})),
        Some(json!({"direction": "sideways"})),
        Some(json!({"direction": 5})),
        Some(json!({"from_actor": 5})),
        Some(json!({"to_actor": "a", "from_actor": "a"})),
        // `json_extract` renders an array or object as JSON text, which an
        // actor id below spells; the row-level rule reads only strings.
        Some(json!({"direction": "inbound", "to_actor": ["x"]})),
        Some(json!({"direction": "outbound", "from_actor": ["x"]})),
        Some(json!({"direction": "inbound", "to_actor": {"k": "v"}})),
    ];
    let mut notes = Vec::new();
    for (i, shape) in shapes.into_iter().enumerate() {
        let mut note = Note::new("local", "message", format!("shape {i}"));
        note.properties = shape;
        notes.push(note);
    }
    let mut other_kind = Note::new("local", "observation", "not a message");
    other_kind.properties = Some(json!({"direction": "inbound", "to_actor": "a"}));
    notes.push(other_kind);
    store
        .upsert_notes(notes.clone())
        .await
        .expect("upsert shapes");

    for (actor_id, delegated) in [
        ("local", false),
        ("local", true),
        ("a", false),
        ("a", true),
        (r#"["x"]"#, false),
        (r#"["x"]"#, true),
        (r#"{"k":"v"}"#, true),
    ] {
        let view = MailboxView {
            actor_id: actor_id.to_string(),
            delegated,
        };
        let filter = NoteFilter {
            mailbox: Some(view.note_scope(&token)),
            ..Default::default()
        };
        assert_eq!(
            filter.mailbox,
            Some(NoteMailboxScope {
                actor_id: actor_id.to_string(),
                legacy_local: !delegated,
            })
        );
        let mut from_store: Vec<String> = rt
            .list_notes_filtered(&token, filter, 100, 0)
            .await
            .expect("filtered list")
            .into_iter()
            .map(|note| note.id.to_string())
            .collect();
        let mut from_rule: Vec<String> = notes
            .iter()
            .filter(|note| view.permits_message_note(&token, note))
            .map(|note| note.id.to_string())
            .collect();
        from_store.sort();
        from_rule.sort();
        assert_eq!(
            from_store, from_rule,
            "store and row-level rule disagree for actor={actor_id} delegated={delegated}"
        );
    }
}

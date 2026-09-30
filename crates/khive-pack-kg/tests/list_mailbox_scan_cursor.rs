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

async fn offset_page(
    registry: &VerbRegistry,
    kind: &str,
    offset: usize,
    limit: usize,
    hidden: &HashSet<String>,
) -> (usize, bool) {
    let page = registry
        .dispatch(
            "list",
            json!({"kind": kind, "offset": offset, "limit": limit}),
        )
        .await
        .expect("list page");
    let rows = page["items"].as_array().expect("items array");
    for row in rows {
        let id = row["id"].as_str().expect("row id");
        assert!(!hidden.contains(id), "hidden row returned: {id}");
    }
    (rows.len(), page["has_more"].as_bool().expect("has_more"))
}

/// Rows past the scan ceiling stay reachable by offset. The partition runs in
/// the store, so a list with no row-level filter pages by the store's offset.
#[tokio::test]
async fn offset_list_reaches_rows_past_the_scan_ceiling() {
    const VISIBLE: usize = 10_005;
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    let store = rt.notes(&token).expect("note store");
    let base = 1_700_000_000_000_000i64;
    let mut hidden_ids = HashSet::new();
    let mut batch = Vec::with_capacity(1000);
    for i in 0..VISIBLE {
        let time = base + i as i64;
        batch.push(visible_message(time, false));
        if i % 500 == 0 {
            let note = hidden_message(i, time, false);
            hidden_ids.insert(note.id.to_string());
            batch.push(note);
        }
        if batch.len() >= 1000 {
            store
                .upsert_notes(std::mem::take(&mut batch))
                .await
                .expect("upsert batch");
        }
    }
    store.upsert_notes(batch).await.expect("upsert tail");
    let reg = registry(&rt);
    // A kg-only registry cannot resolve kind="message"; the generic note list
    // takes the same paging branch as an explicit message list.
    assert_eq!(
        offset_page(&reg, "note", 9_990, 5, &hidden_ids).await,
        (5, true),
        "offset=9990"
    );
    assert_eq!(
        offset_page(&reg, "note", 10_000, 20, &hidden_ids).await,
        (5, false),
        "offset=10000"
    );
    assert_eq!(
        offset_page(&reg, "note", VISIBLE, 20, &hidden_ids).await,
        (0, false),
        "offset past the end"
    );
}

async fn list_refusal(registry: &VerbRegistry, args: Value) -> String {
    format!(
        "{:?}",
        registry
            .dispatch("list", args)
            .await
            .expect_err("list refuses the anchor")
    )
}

/// A caller-supplied anchor naming a message this mailbox hides answers
/// exactly as an anchor naming nothing does, so neither `after` nor
/// `after_key` probes for another actor's mail.
#[tokio::test]
async fn hidden_anchor_answers_as_a_missing_one() {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let token = rt.authorize(Namespace::local()).expect("authorize local");
    let store = rt.notes(&token).expect("note store");
    let base = 1_700_000_000_000_000i64;
    let hidden = hidden_message(1, base, true);
    let visible = visible_message(base + 1, true);
    // An observation sharing a hidden message's key. Counted together, the
    // two rows refuse as ambiguous and the refusal names the message kind.
    let hidden_twin = hidden_message(0, base, true);
    let mut twin = Note::new("local", "observation", "twin");
    twin.key = hidden_twin.key.clone();
    let hidden_id = hidden.id.to_string();
    let visible_id = visible.id.to_string();
    let missing_id = Note::new("local", "message", "never stored").id.to_string();
    store
        .upsert_notes(vec![hidden, visible, hidden_twin, twin])
        .await
        .expect("seed");
    let reg = registry(&rt);

    reg.dispatch(
        "list",
        json!({"kind": "note", "after": visible_id, "limit": 5}),
    )
    .await
    .expect("a visible anchor pages");
    let hidden_err = list_refusal(
        &reg,
        json!({"kind": "note", "after": hidden_id, "limit": 5}),
    )
    .await;
    let missing_err = list_refusal(
        &reg,
        json!({"kind": "note", "after": missing_id, "limit": 5}),
    )
    .await;
    assert_eq!(
        hidden_err.replace(&hidden_id, "<id>"),
        missing_err.replace(&missing_id, "<id>"),
        "after"
    );

    let keyed = |key: &str| json!({"kind": "note", "key_prefix": "", "after_key": key, "limit": 5});
    let hidden_err = list_refusal(&reg, keyed("hidden/00001")).await;
    let missing_err = list_refusal(&reg, keyed("hidden/99999")).await;
    assert_eq!(
        hidden_err.replace("hidden/00001", "<key>"),
        missing_err.replace("hidden/99999", "<key>"),
        "after_key"
    );
    reg.dispatch("list", keyed("hidden/00000"))
        .await
        .expect("a key shared with a hidden message resolves to the visible row");
}

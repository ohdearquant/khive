use std::collections::BTreeSet;
use std::sync::Arc;

use khive_runtime::{
    AllowAllGate, BackendId, KhiveRuntime, Namespace, RuntimeConfig, VerbRegistry,
    VerbRegistryBuilder,
};
use khive_storage::Note;
use serde_json::{json, Value};

use crate::CommPack;

fn actor_registry(
    backend: Arc<khive_db::StorageBackend>,
    actor: &str,
) -> (VerbRegistry, KhiveRuntime) {
    let runtime = KhiveRuntime::from_backend(
        backend,
        RuntimeConfig {
            mounts: Vec::new(),
            exec: Default::default(),
            git_write: Default::default(),
            display_timezone: khive_runtime::config::resolve_default_display_timezone(),
            events_split: None,
            db_path: None,
            blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
            default_namespace: Namespace::local(),
            embedding_model: None,
            additional_embedding_models: vec![],
            gate: Arc::new(AllowAllGate),
            packs: vec!["kg".to_string(), "comm".to_string()],
            backend_id: BackendId::main(),
            brain_profile: None,
            visible_namespaces: vec![],
            allowed_outbound_namespaces: vec![],
            actor_id: Some(actor.to_string()),
        },
    );
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(CommPack::new(runtime.clone()));
    builder.with_actor_id(Some(actor.to_string()));
    (builder.build().expect("actor registry"), runtime)
}

fn parties() -> (VerbRegistry, VerbRegistry, KhiveRuntime) {
    let backend = khive_db::StorageBackend::memory().expect("in-memory backend");
    {
        let mut writer = backend.pool().try_writer().expect("migration writer");
        khive_db::run_migrations(writer.conn_mut()).expect("migrations");
    }
    let backend = Arc::new(backend);
    let (sender, runtime) = actor_registry(Arc::clone(&backend), "actor:sender");
    let (recipient, _) = actor_registry(backend, "actor:recipient");
    (sender, recipient, runtime)
}

async fn send(
    registry: &VerbRegistry,
    content: &str,
    tags: Value,
    thread_id: Option<&str>,
) -> Value {
    registry
        .dispatch(
            "comm.send",
            json!({
                "to": "actor:recipient",
                "subject": content,
                "content": content,
                "tags": tags,
                "thread_id": thread_id,
            }),
        )
        .await
        .expect("send fixture message")
}

fn inbox_params(mailbox: &str, filters: Value) -> Value {
    let mut params = json!({"box": mailbox});
    if mailbox == "inbox" {
        params["status"] = json!("all");
    }
    params
        .as_object_mut()
        .unwrap()
        .extend(filters.as_object().unwrap().clone());
    params
}

async fn inbox(registry: &VerbRegistry, mailbox: &str, filters: Value) -> Value {
    registry
        .dispatch("comm.inbox", inbox_params(mailbox, filters))
        .await
        .expect("filtered mailbox")
}

fn content_set(response: &Value) -> BTreeSet<&str> {
    response["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .map(|message| message["content"].as_str().expect("message content"))
        .collect()
}

#[tokio::test]
async fn inbox_tags_are_exact_all_of_in_both_boxes_and_preserve_id_pair() {
    let (sender, recipient, _) = parties();
    let sent = send(
        &sender,
        "matching message",
        json!(["mail_id:abc", "scope:release"]),
        None,
    )
    .await;
    for (content, tags) in [
        ("first tag only", json!(["mail_id:abc"])),
        ("prefix decoy", json!(["mail_id:abcd", "scope:release"])),
        ("case decoy", json!(["MAIL_ID:abc", "scope:release"])),
        ("second tag only", json!(["scope:release"])),
        ("missing tags", Value::Null),
        ("empty array", json!([])),
        ("empty string", json!([""])),
    ] {
        send(&sender, content, tags, None).await;
    }

    for (registry, mailbox) in [(&sender, "sent"), (&recipient, "inbox")] {
        let all = inbox(registry, mailbox, json!({})).await;
        assert_eq!(all["count"], 8, "unfiltered decoy control: {all}");

        let filtered = inbox(
            registry,
            mailbox,
            json!({"tags": ["mail_id:abc", "scope:release"]}),
        )
        .await;
        assert_eq!(
            content_set(&filtered),
            BTreeSet::from(["matching message"]),
            "tag predicate must reject every decoy in {mailbox}: {filtered}"
        );
        let message = &filtered["messages"][0];
        assert_eq!(
            message["properties"]["tags"],
            json!(["mail_id:abc", "scope:release"])
        );
        if mailbox == "sent" {
            assert_eq!(message["full_id"], sent["full_id"]);
        } else {
            assert_ne!(message["full_id"], sent["full_id"]);
            assert_eq!(message["properties"]["outbound_ref"], sent["full_id"]);
        }

        let first_only = inbox(registry, mailbox, json!({"tags": ["mail_id:abc"]})).await;
        assert_eq!(
            content_set(&first_only),
            BTreeSet::from(["matching message", "first tag only"])
        );
        let duplicate = inbox(
            registry,
            mailbox,
            json!({"tags": ["mail_id:abc", "mail_id:abc", "scope:release"]}),
        )
        .await;
        assert_eq!(
            content_set(&duplicate),
            BTreeSet::from(["matching message"])
        );

        for tags in [
            json!(["mail_id:missing"]),
            json!(["mail_id:abc", "scope:RELEASE"]),
        ] {
            let absent = inbox(registry, mailbox, json!({"tags": tags})).await;
            assert_eq!(absent["count"], 0, "exact absent-tag control: {absent}");
        }
        for tags in [json!([]), Value::Null] {
            let unfiltered = inbox(registry, mailbox, json!({"tags": tags})).await;
            assert_eq!(content_set(&unfiltered), content_set(&all));
        }
        let empty_string = inbox(registry, mailbox, json!({"tags": [""]})).await;
        assert_eq!(content_set(&empty_string), BTreeSet::from(["empty string"]));
    }

    let inbound = inbox(
        &recipient,
        "inbox",
        json!({"tags": ["mail_id:abc", "scope:release"]}),
    )
    .await;
    let outbound_error = sender
        .dispatch("comm.read", json!({"id": sent["full_id"]}))
        .await
        .expect_err("outbound UUID cannot mark an inbound message read");
    assert!(outbound_error.to_string().contains("outbound"));
    let read = recipient
        .dispatch(
            "comm.read",
            json!({"id": inbound["messages"][0]["full_id"]}),
        )
        .await
        .expect("recipient reads the inbound UUID");
    assert_eq!(read["read"], true);
}

#[tokio::test]
async fn inbox_kind_and_thread_filters_intersect_with_tags_in_both_boxes() {
    let (sender, recipient, _) = parties();
    let root = send(&sender, "thread root", json!(["selected"]), None).await;
    let thread_id = root["thread_id"].as_str().expect("canonical thread UUID");
    send(
        &sender,
        "selected continuation",
        json!(["selected", "continuation", "reply"]),
        Some(thread_id),
    )
    .await;
    send(
        &sender,
        "untagged continuation",
        Value::Null,
        Some(thread_id),
    )
    .await;
    let other = send(&sender, "other thread", json!(["selected"]), None).await;

    for (registry, mailbox) in [(&sender, "sent"), (&recipient, "inbox")] {
        let native_kind = inbox(registry, mailbox, json!({"kind": "message"})).await;
        assert_eq!(native_kind["count"], 4);
        let application_kind = inbox(registry, mailbox, json!({"kind": "reply"})).await;
        assert_eq!(application_kind["count"], 0);

        let thread = inbox(registry, mailbox, json!({"thread_id": thread_id})).await;
        assert_eq!(
            content_set(&thread),
            BTreeSet::from([
                "thread root",
                "selected continuation",
                "untagged continuation"
            ])
        );
        let combined = inbox(
            registry,
            mailbox,
            json!({"kind": "message", "thread_id": thread_id, "tags": ["selected"]}),
        )
        .await;
        assert_eq!(
            content_set(&combined),
            BTreeSet::from(["thread root", "selected continuation"])
        );
        let other_thread = inbox(
            registry,
            mailbox,
            json!({"kind": "message", "thread_id": other["thread_id"], "tags": ["selected"]}),
        )
        .await;
        assert_eq!(content_set(&other_thread), BTreeSet::from(["other thread"]));
        let disjoint = inbox(
            registry,
            mailbox,
            json!({"kind": "message", "thread_id": other["thread_id"], "tags": ["continuation"]}),
        )
        .await;
        assert_eq!(disjoint["count"], 0);
    }
}

#[tokio::test]
async fn inbox_tag_filters_precede_limit_and_offset_across_store_pages_in_both_boxes() {
    let (sender, recipient, runtime) = parties();
    let token = runtime.authorize(Namespace::local()).expect("local token");
    let store = runtime.notes(&token).expect("notes store");
    let base = chrono::DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
        .unwrap()
        .timestamp_micros();
    let thread_id = uuid::Uuid::new_v4().to_string();

    // More than a complete internal page of newer decoys precedes every match.
    for sequence in 1..=210u32 {
        for direction in ["outbound", "inbound"] {
            let mut note = Note::new("local", "message", format!("message {sequence}"))
                .with_properties(json!({
                    "direction": direction,
                    "from_actor": "actor:sender",
                    "to_actor": "actor:recipient",
                    "read": false,
                    "subject": "batch MATCH",
                    "thread_id": thread_id,
                    "tags": if sequence <= 5 {
                        json!(["mail_id:page", "batch:one"])
                    } else {
                        json!(["mail_id:page-decoy", "batch:one"])
                    },
                }));
            note.created_at = base + i64::from(sequence) * 1_000_000;
            note.updated_at = note.created_at;
            store
                .upsert_note(note)
                .await
                .expect("insert ordered message");
        }
    }
    drop(store);

    for (registry, mailbox) in [(&sender, "sent"), (&recipient, "inbox")] {
        let control = inbox(registry, mailbox, json!({"limit": 2})).await;
        assert_eq!(control["messages"][0]["content"], "message 210");

        for (offset, expected, next_offset) in [
            (0, vec!["message 5", "message 4"], json!(2)),
            (2, vec!["message 3", "message 2"], json!(4)),
            (4, vec!["message 1"], Value::Null),
        ] {
            let page = inbox(
                registry,
                mailbox,
                json!({
                    "tags": ["mail_id:page", "batch:one"],
                    "kind": "message",
                    "thread_id": thread_id,
                    "limit": 2,
                    "offset": offset,
                }),
            )
            .await;
            let contents: Vec<&str> = page["messages"]
                .as_array()
                .unwrap()
                .iter()
                .map(|message| message["content"].as_str().unwrap())
                .collect();
            assert_eq!(contents, expected, "filtered page in {mailbox}: {page}");
            assert_eq!(page["count"], expected.len());
            assert_eq!(page["offset"], offset);
            assert_eq!(page["next_offset"], next_offset);
            assert_eq!(page["has_more"], !next_offset.is_null());
        }

        let after_end = inbox(
            registry,
            mailbox,
            json!({"tags": ["mail_id:page"], "offset": 5, "limit": 2}),
        )
        .await;
        assert_eq!(after_end["count"], 0);
        assert_eq!(after_end["has_more"], false);
        assert_eq!(after_end["next_offset"], Value::Null);

        let bounded = inbox(
            registry,
            mailbox,
            json!({
                "tags": ["mail_id:page", "batch:one"],
                "since": "2026-09-01T00:00:03Z",
                "subject_contains": "mAtCh",
                "limit": 10,
            }),
        )
        .await;
        assert_eq!(
            content_set(&bounded),
            BTreeSet::from(["message 3", "message 4", "message 5"])
        );

        let wrong_subject = inbox(
            registry,
            mailbox,
            json!({"tags": ["mail_id:page"], "subject_contains": "absent subject"}),
        )
        .await;
        assert_eq!(wrong_subject["count"], 0);

        let zero_limit = inbox(
            registry,
            mailbox,
            json!({"tags": ["mail_id:page"], "limit": 0}),
        )
        .await;
        assert_eq!(zero_limit["count"], 0);
        assert_eq!(zero_limit["next_offset"], Value::Null);
        assert_eq!(zero_limit["has_more"], false);
    }
}

#[tokio::test]
async fn inbox_rejects_malformed_tag_filter_types_in_both_boxes() {
    let (sender, recipient, _) = parties();
    for (registry, mailbox) in [(&sender, "sent"), (&recipient, "inbox")] {
        for tags in [
            json!("mail_id:abc"),
            json!(1),
            json!(true),
            json!({"tag": "mail_id:abc"}),
            json!([null]),
            json!([1]),
            json!(["mail_id:abc", false]),
        ] {
            let error = registry
                .dispatch("comm.inbox", inbox_params(mailbox, json!({"tags": tags})))
                .await
                .expect_err("tags must be an array containing only strings");
            assert!(
                error.to_string().contains("invalid type"),
                "malformed tags must fail type validation, not unknown-field validation: {error}"
            );
        }
    }
}

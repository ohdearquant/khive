use std::sync::Arc;

use crate::CommPack;
use khive_runtime::RuntimeError;
use khive_runtime::{
    AllowAllGate, BackendId, KhiveRuntime, Namespace, RuntimeConfig, VerbRegistry,
    VerbRegistryBuilder,
};
use khive_storage::types::DeleteMode;
use khive_storage::{SqlStatement, SqlValue};
use serde_json::json;
use serde_json::Value;
use uuid::Uuid;

fn actor_registry(
    backend: Arc<khive_db::StorageBackend>,
    actor: &str,
    namespace: &str,
) -> (VerbRegistry, KhiveRuntime) {
    let runtime = KhiveRuntime::from_backend(
        backend,
        RuntimeConfig {
            mounts: Vec::new(),
            git_write: Default::default(),
            exec: Default::default(),
            display_timezone: khive_runtime::config::resolve_default_display_timezone(),
            events_split: None,
            db_path: None,
            blob_hydration_bytes: khive_runtime::DEFAULT_BLOB_HYDRATION_BYTES,
            default_namespace: Namespace::parse(namespace).unwrap(),
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
    builder.with_default_namespace(namespace);
    (builder.build().expect("actor registry"), runtime)
}

fn parties() -> (VerbRegistry, VerbRegistry, KhiveRuntime) {
    let backend = khive_db::StorageBackend::memory().expect("in-memory backend");
    {
        let mut writer = backend.pool().try_writer().expect("migration writer");
        khive_db::run_migrations(writer.conn_mut()).expect("migrations");
    }
    let backend = Arc::new(backend);
    let (sender, runtime) = actor_registry(Arc::clone(&backend), "actor:sender", "local");
    let (recipient, _) = actor_registry(backend, "actor:recipient", "local");
    (sender, recipient, runtime)
}

fn file_parties() -> (tempfile::TempDir, VerbRegistry, VerbRegistry, KhiveRuntime) {
    let dir = tempfile::tempdir().unwrap();
    let backend = khive_db::StorageBackend::sqlite(dir.path().join("messages.db")).unwrap();
    {
        let mut writer = backend.pool().try_writer().unwrap();
        khive_db::run_migrations(writer.conn_mut()).unwrap();
    }
    assert!(backend.pool().writer_task_handle().unwrap().is_some());
    let backend = Arc::new(backend);
    let (sender, runtime) = actor_registry(Arc::clone(&backend), "actor:sender", "local");
    let (recipient, _) = actor_registry(backend, "actor:recipient", "local");
    (dir, sender, recipient, runtime)
}

async fn message_count(runtime: &KhiveRuntime, keyed: bool) -> i64 {
    let sql = if keyed {
        "SELECT count(*) FROM notes WHERE kind = 'message' AND key IS NOT NULL AND deleted_at IS NULL"
    } else {
        "SELECT count(*) FROM notes WHERE kind = 'message' AND deleted_at IS NULL"
    };
    let result = runtime
        .sql()
        .reader()
        .await
        .unwrap()
        .query_scalar(SqlStatement {
            sql: sql.into(),
            params: vec![],
            label: Some("idempotency-population".into()),
        })
        .await
        .unwrap();
    match result {
        Some(SqlValue::Integer(n)) => n,
        other => panic!("unexpected count: {other:?}"),
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn idempotency_send_replay_preserves_pair_population_and_both_ids() {
    let (sender, recipient, runtime) = parties();
    let args = json!({"to":"actor:recipient","subject":"subject","content":"same mail","idempotency_key":"operation-one"});
    let original = sender
        .dispatch("comm.send", args.clone())
        .await
        .expect("first keyed send");
    assert_eq!(message_count(&runtime, false).await, 2);
    let replay = sender
        .dispatch("comm.send", args)
        .await
        .expect("lost-ACK replay");
    assert_eq!(
        message_count(&runtime, false).await,
        2,
        "replay must not create a second pair"
    );
    assert_eq!(
        message_count(&runtime, true).await,
        1,
        "only outbound claims the key"
    );
    assert_eq!(original["replayed"], false);
    assert_eq!(replay["replayed"], true);
    for field in [
        "full_id",
        "recipient_id",
        "sent_at",
        "thread_id",
        "idempotency_key",
    ] {
        assert_eq!(
            replay[field], original[field],
            "stable receipt field {field}"
        );
        assert!(!replay[field].is_null(), "receipt field {field} must exist");
    }
    for (registry, box_name, expected_id) in [
        (&sender, "sent", &original["full_id"]),
        (&recipient, "inbox", &original["recipient_id"]),
    ] {
        let mut params = json!({"box": box_name});
        if box_name == "inbox" {
            params["status"] = json!("all");
        }
        let result = registry.dispatch("comm.inbox", params).await.unwrap();
        assert_eq!(result["count"], 1);
        let row = &result["messages"][0];
        assert_eq!(&row["full_id"], expected_id);
        assert_eq!(row["properties"]["idempotency_key"], "operation-one");
    }
}

fn args(key: &str) -> Value {
    json!({"to":"actor:recipient","subject":"subject","content":"same mail","idempotency_key":key})
}

async fn snapshot(runtime: &KhiveRuntime) -> String {
    let value = runtime.sql().reader().await.unwrap().query_scalar(SqlStatement {
        sql: "SELECT json_group_array(json_object('id',id,'content',content,'properties',properties,'key',key,'created_at',created_at,'updated_at',updated_at,'deleted_at',deleted_at)) FROM (SELECT * FROM notes WHERE kind='message' ORDER BY id)".into(),
        params: vec![], label: Some("idempotency-domain-snapshot".into()),
    }).await.unwrap();
    match value {
        Some(SqlValue::Text(value)) => value,
        other => panic!("unexpected domain snapshot: {other:?}"),
    }
}

fn conflict(error: RuntimeError, expected_id: &Value) {
    let RuntimeError::Khive(error) = error else {
        panic!("expected structured key conflict: {error:?}")
    };
    assert_eq!(error.kind(), khive_types::ErrorKind::Conflict);
    let details = error.details().unwrap();
    assert_eq!(details.get("reason"), Some("key_conflict"));
    assert_eq!(details.get("existing_id"), expected_id.as_str());
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn idempotency_conflicts_preserve_all_message_rows() {
    let (sender, _, runtime) = parties();
    let original = sender.dispatch("comm.send", args("payload")).await.unwrap();
    let before = snapshot(&runtime).await;
    for (field, value) in [
        ("content", json!("different")),
        ("subject", Value::Null),
        ("to", json!("actor:else")),
        ("tags", json!(["different"])),
        ("thread_id", original["full_id"].clone()),
    ] {
        let mut changed = args("payload");
        changed[field] = value;
        conflict(
            sender.dispatch("comm.send", changed).await.unwrap_err(),
            &original["full_id"],
        );
        assert_eq!(
            snapshot(&runtime).await,
            before,
            "conflict mutated domain for {field}"
        );
    }
    let mut equivalent = args("payload");
    equivalent["to"] = json!(" actor:recipient ");
    equivalent["tags"] = json!([]);
    let replay = sender.dispatch("comm.send", equivalent).await.unwrap();
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["full_id"], original["full_id"]);
    assert_eq!(snapshot(&runtime).await, before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(config_ledger)]
async fn idempotency_concurrent_sends_leave_one_pair_and_no_fts_orphans() {
    let (_dir, sender, _, runtime) = file_parties();
    for round in 0..8 {
        let params = args(&format!("concurrent-{round}"));
        let (left, right) = tokio::join!(
            sender.dispatch("comm.send", params.clone()),
            sender.dispatch("comm.send", params)
        );
        let (left, right) = (left.unwrap(), right.unwrap());
        assert_eq!(left["full_id"], right["full_id"]);
        assert_eq!(left["recipient_id"], right["recipient_id"]);
        assert_ne!(left["replayed"], right["replayed"]);
        assert_eq!(message_count(&runtime, false).await, (round + 1) * 2);
        assert_eq!(message_count(&runtime, true).await, round + 1);
    }
    for table in ["fts_notes", "fts_notes_rowids"] {
        let count = runtime
            .sql()
            .reader()
            .await
            .unwrap()
            .query_scalar(SqlStatement {
                sql: format!("SELECT count(*) FROM {table}"),
                params: vec![],
                label: Some("idempotency-fts-population".into()),
            })
            .await
            .unwrap();
        assert!(
            matches!(count, Some(SqlValue::Integer(16))),
            "orphan rows in {table}: {count:?}"
        );
    }
}

async fn parent(sender: &VerbRegistry, recipient: &VerbRegistry) -> String {
    let sent = sender
        .dispatch(
            "comm.send",
            json!({"to":"actor:recipient","subject":"parent","content":"parent"}),
        )
        .await
        .unwrap();
    let rows = recipient
        .dispatch(
            "comm.inbox",
            json!({"status":"all","thread_id":sent["thread_id"]}),
        )
        .await
        .unwrap();
    rows["messages"][0]["full_id"].as_str().unwrap().to_owned()
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn idempotency_reply_replays_alias_without_marking_parent_again() {
    let (sender, recipient, runtime) = parties();
    let parent_id = parent(&sender, &recipient).await;
    let params = json!({"id":parent_id,"content":"answer","idempotency_key":"reply-key"});
    let original = recipient
        .dispatch("comm.reply", params.clone())
        .await
        .unwrap();
    assert_eq!(original["marked_read"], true);
    let before = snapshot(&runtime).await;
    let mut alias = params.clone();
    alias["id"] = json!(&parent_id[..8]);
    let replay = recipient.dispatch("comm.reply", alias).await.unwrap();
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["full_id"], original["full_id"]);
    assert_eq!(replay["recipient_id"], original["recipient_id"]);
    assert!(replay["marked_read"].is_null());
    assert_eq!(
        snapshot(&runtime).await,
        before,
        "replay repeated parent mark-read"
    );
    let mut changed = params;
    changed["content"] = json!("other answer");
    conflict(
        recipient.dispatch("comm.reply", changed).await.unwrap_err(),
        &original["full_id"],
    );
    assert_eq!(snapshot(&runtime).await, before);
    let other_parent = parent(&sender, &recipient).await;
    let before = snapshot(&runtime).await;
    conflict(
        recipient
            .dispatch(
                "comm.reply",
                json!({"id":other_parent,"content":"answer","idempotency_key":"reply-key"}),
            )
            .await
            .unwrap_err(),
        &original["full_id"],
    );
    conflict(recipient.dispatch("comm.send",json!({"to":"actor:sender","subject":"Re: parent","content":"answer","thread_id":original["thread_id"],"idempotency_key":"reply-key"})).await.unwrap_err(),&original["full_id"]);
    assert_eq!(
        snapshot(&runtime).await,
        before,
        "conflicting reply/send wrote notes"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(config_ledger)]
async fn idempotency_concurrent_replies_preserve_one_response_pair() {
    let (_dir, sender, recipient, runtime) = file_parties();
    let parent_id = parent(&sender, &recipient).await;
    let params = json!({"id":parent_id,"content":"answer","idempotency_key":"reply-race"});
    let (left, right) = tokio::join!(
        recipient.dispatch("comm.reply", params.clone()),
        recipient.dispatch("comm.reply", params)
    );
    let (left, right) = (left.unwrap(), right.unwrap());
    assert_eq!(left["full_id"], right["full_id"]);
    assert_eq!(left["recipient_id"], right["recipient_id"]);
    assert_ne!(left["replayed"], right["replayed"]);
    assert_eq!(message_count(&runtime, false).await, 4);
    assert_eq!(message_count(&runtime, true).await, 1);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn idempotency_actor_namespace_and_delimiter_tuples_are_distinct() {
    let backend = khive_db::StorageBackend::memory().unwrap();
    {
        let mut writer = backend.pool().try_writer().unwrap();
        khive_db::run_migrations(writer.conn_mut()).unwrap();
    }
    let backend = Arc::new(backend);
    let (a, runtime) = actor_registry(Arc::clone(&backend), "actor:a", "local");
    let (b, _) = actor_registry(Arc::clone(&backend), "actor:a:b", "local");
    let (c, _) = actor_registry(backend, "actor:a", "other-ns");
    let mut ids = std::collections::BTreeSet::new();
    for (registry, key, namespace) in [
        (&a, "b:c", "local"),
        (&b, "c", "local"),
        (&b, "b:c", "local"),
        (&c, "b:c", "other-ns"),
    ] {
        let mut params = args(key);
        params["namespace"] = json!(namespace);
        let first = registry
            .dispatch("comm.send", params.clone())
            .await
            .unwrap();
        assert!(ids.insert(first["full_id"].as_str().unwrap().to_owned()));
        let again = registry.dispatch("comm.send", params).await.unwrap();
        assert_eq!(again["full_id"], first["full_id"]);
        assert_eq!(again["replayed"], true);
    }
    assert_eq!(message_count(&runtime, false).await, 8);
    assert_eq!(message_count(&runtime, true).await, 4);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn idempotency_key_validation_self_send_and_unkeyed_controls() {
    let (sender, _, runtime) = parties();
    for key in ["k".repeat(513), "λ".repeat(257), "nul\0key".into()] {
        assert!(sender.dispatch("comm.send", args(&key)).await.is_err());
        assert_eq!(message_count(&runtime, false).await, 0);
    }
    for key in [
        String::new(),
        "k".repeat(512),
        "λ".repeat(256),
        "line\n\t\rkey".into(),
    ] {
        let first = sender.dispatch("comm.send", args(&key)).await.unwrap();
        let again = sender.dispatch("comm.send", args(&key)).await.unwrap();
        assert_eq!(first["full_id"], again["full_id"]);
        assert_eq!(again["idempotency_key"], key);
    }
    let self_args =
        json!({"to":"actor:sender","content":"self","self_send":true,"idempotency_key":"self-key"});
    let first = sender
        .dispatch("comm.send", self_args.clone())
        .await
        .unwrap();
    let again = sender.dispatch("comm.send", self_args).await.unwrap();
    assert_eq!(first["full_id"], again["full_id"]);
    let before = message_count(&runtime, false).await;
    let unkeyed = json!({"to":"actor:recipient","content":"intentional repeat"});
    let first = sender.dispatch("comm.send", unkeyed.clone()).await.unwrap();
    let second = sender.dispatch("comm.send", unkeyed).await.unwrap();
    assert_ne!(first["full_id"], second["full_id"]);
    assert!(first.get("replayed").is_none());
    assert_eq!(message_count(&runtime, false).await, before + 4);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn idempotency_missing_or_altered_sibling_refuses_without_repair() {
    for damage in [
        "missing",
        "content",
        "thread",
        "reference",
        "actor",
        "physical-key",
    ] {
        let (sender, _, runtime) = parties();
        let first = sender.dispatch("comm.send", args("intact")).await.unwrap();
        let token = runtime.authorize(Namespace::local()).unwrap();
        let store = runtime.notes(&token).unwrap();
        let recipient = first["recipient_id"]
            .as_str()
            .unwrap()
            .parse::<Uuid>()
            .unwrap();
        if damage == "missing" {
            assert!(store
                .delete_note(recipient, DeleteMode::Hard)
                .await
                .unwrap());
        } else {
            let mut note = store.get_note(recipient).await.unwrap().unwrap();
            match damage {
                "content" => note.content = "tampered".into(),
                "thread" => note.properties.as_mut().unwrap()["thread_id"] = json!(Uuid::new_v4()),
                "reference" => {
                    note.properties.as_mut().unwrap()["outbound_ref"] = json!(Uuid::new_v4())
                }
                "actor" => note.properties.as_mut().unwrap()["to_actor"] = json!("actor:else"),
                "physical-key" => {
                    // Ordinary UPSERT deliberately preserves an existing key.
                    // Corrupt the isolated fixture through SQL and verify it below.
                    runtime
                        .sql()
                        .writer()
                        .await
                        .unwrap()
                        .execute(SqlStatement {
                            sql:
                                "UPDATE notes SET key = 'unexpected-recipient-claim' WHERE id = ?1"
                                    .into(),
                            params: vec![SqlValue::Text(recipient.to_string())],
                            label: Some("corrupt-test-sibling-key".into()),
                        })
                        .await
                        .unwrap();
                }
                _ => unreachable!(),
            }
            store.upsert_note(note).await.unwrap();
        }
        if damage == "physical-key" {
            assert_eq!(
                store
                    .get_note(recipient)
                    .await
                    .unwrap()
                    .unwrap()
                    .key
                    .as_deref(),
                Some("unexpected-recipient-claim")
            );
        }
        let before = snapshot(&runtime).await;
        conflict(
            sender
                .dispatch("comm.send", args("intact"))
                .await
                .expect_err(&format!("replay accepted {damage} sibling")),
            &first["full_id"],
        );
        assert_eq!(
            snapshot(&runtime).await,
            before,
            "repaired damaged sibling: {damage}"
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn idempotency_outbound_deletion_releases_live_claim() {
    for mode in [DeleteMode::Soft, DeleteMode::Hard] {
        let (sender, _, runtime) = parties();
        let first = sender
            .dispatch("comm.send", args("released"))
            .await
            .unwrap();
        let token = runtime.authorize(Namespace::local()).unwrap();
        assert!(runtime
            .notes(&token)
            .unwrap()
            .delete_note(first["full_id"].as_str().unwrap().parse().unwrap(), mode)
            .await
            .unwrap());
        let next = sender
            .dispatch("comm.send", args("released"))
            .await
            .unwrap();
        assert_ne!(first["full_id"], next["full_id"]);
        assert_eq!(next["replayed"], false);
        assert_eq!(
            message_count(&runtime, false).await,
            3,
            "old inbound remains beside new pair"
        );
        assert_eq!(message_count(&runtime, true).await, 1);
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn idempotency_readback_and_delivery_changes_preserve_replay() {
    let (sender, recipient, runtime) = parties();
    let first = sender
        .dispatch("comm.send", args("read-back"))
        .await
        .unwrap();
    for (registry, mailbox) in [(&sender, "sent"), (&recipient, "inbox")] {
        let result = registry
            .dispatch(
                "comm.inbox",
                json!({"box":mailbox,"fields":["full_id","idempotency_key"]}),
            )
            .await
            .unwrap();
        assert_eq!(result["messages"][0]["idempotency_key"], "read-back");
        let thread = registry
            .dispatch("comm.thread", json!({"id":first["thread_id"]}))
            .await
            .unwrap();
        let messages = thread["messages"].as_array().unwrap();
        assert!(!messages.is_empty());
        assert!(messages
            .iter()
            .all(|note| note["properties"]["idempotency_key"] == "read-back"));
    }
    let read = recipient
        .dispatch("comm.read", json!({"id":first["recipient_id"]}))
        .await
        .unwrap();
    assert_eq!(read["properties"]["idempotency_key"], "read-back");
    assert!(sender
        .dispatch("comm.read", json!({"id":first["full_id"]}))
        .await
        .is_err());
    let token = runtime.authorize(Namespace::local()).unwrap();
    runtime
        .notes(&token)
        .unwrap()
        .set_note_property(
            first["full_id"].as_str().unwrap().parse().unwrap(),
            "delivered_at",
            json!("2026-09-09T00:00:00Z"),
            1,
        )
        .await
        .unwrap();
    let before = snapshot(&runtime).await;
    let replay = sender
        .dispatch("comm.send", args("read-back"))
        .await
        .unwrap();
    assert_eq!(replay["replayed"], true);
    assert_eq!(snapshot(&runtime).await, before);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn idempotency_replay_does_not_publish_an_inbox_signal() {
    let (_, _, runtime) = parties();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let signal = crate::inbox_signal::InboxSignal::new();
    crate::handlers::handle_send(&runtime, &signal, &token, args("signal"))
        .await
        .unwrap();
    assert_eq!(signal.snapshot(), 1);
    let before = snapshot(&runtime).await;
    let replay = crate::handlers::handle_send(&runtime, &signal, &token, args("signal"))
        .await
        .unwrap();
    assert_eq!(replay["replayed"], true);
    assert_eq!(signal.snapshot(), 1);
    assert_eq!(snapshot(&runtime).await, before);
}

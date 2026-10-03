use std::sync::Arc;

use khive_db::{stores::blob::FsBlobStore, StorageBackend};
use khive_pack_comm as _;
use khive_pack_kg as _;
use khive_runtime::{
    BackendId, KhiveRuntime, Namespace, PackRegistry, RequestIdentity, RuntimeConfig, RuntimeError,
    VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{
    Attachment, AttachmentSubstrate, BlobStore, ContentRef, NewAttachment, SqlStatement, SqlValue,
};
use serde_json::{json, Value};
use uuid::Uuid;

const SENDER: &str = "actor:sender";
const RECIPIENT: &str = "actor:recipient";
const NAMESPACE: &str = "local";

struct Fixture {
    registry: VerbRegistry,
    runtime: KhiveRuntime,
    blobs: Arc<FsBlobStore>,
    _root: tempfile::TempDir,
}

#[derive(Debug, PartialEq, Eq)]
struct Population {
    notes: i64,
    attachments: i64,
}

fn identity(actor: &str) -> RequestIdentity {
    RequestIdentity {
        namespace: NAMESPACE.into(),
        actor_id: Some(actor.into()),
        ..Default::default()
    }
}

fn registry(runtime: &KhiveRuntime) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.with_default_namespace(NAMESPACE);
    PackRegistry::register_packs(&["kg".into(), "comm".into()], runtime.clone(), &mut builder)
        .expect("register local message packs");
    builder.build().expect("local message registry")
}

fn fixture(split_comm: bool) -> Fixture {
    let runtime = if split_comm {
        let main = Arc::new(StorageBackend::memory().expect("main backend"));
        let comm = Arc::new(StorageBackend::memory().expect("comm backend"));
        main.prepare_core_schema().expect("main schema");
        comm.prepare_core_schema().expect("comm schema");
        let config = RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::local(),
            backend_id: BackendId::parse("commfiles").expect("comm backend identity"),
            ..RuntimeConfig::no_embeddings()
        };
        KhiveRuntime::from_backend(comm, config).with_core_backend(main)
    } else {
        KhiveRuntime::memory().expect("main message runtime")
    };
    let root = tempfile::tempdir().expect("private blob root");
    let blobs = Arc::new(FsBlobStore::new(root.path().to_path_buf(), 0).expect("blob store"));
    runtime
        .install_blob_store(blobs.clone())
        .expect("install real message blob store");
    Fixture {
        registry: registry(&runtime),
        runtime,
        blobs,
        _root: root,
    }
}

async fn dispatch(fixture: &Fixture, actor: &str, verb: &str, params: Value) -> Value {
    fixture
        .registry
        .dispatch_with_identity(verb, params, Some(identity(actor)))
        .await
        .unwrap_or_else(|error| panic!("{verb} positive dispatch must succeed: {error}"))
}

async fn publish(fixture: &Fixture, count: usize) -> Vec<(ContentRef, u64)> {
    let mut objects = Vec::new();
    for index in 0..count {
        let bytes = vec![b'a' + index as u8; 11 + index];
        let size = bytes.len() as u64;
        let reference = fixture
            .blobs
            .put(bytes)
            .await
            .expect("publish fixture blob");
        assert_eq!(
            fixture
                .blobs
                .size(&reference)
                .await
                .expect("stat fixture blob"),
            Some(size),
            "the actual blob store supplies attachment sizes"
        );
        objects.push((reference, size));
    }
    objects
}

fn refs(objects: &[(ContentRef, u64)]) -> Vec<&str> {
    objects
        .iter()
        .map(|(reference, _)| reference.as_str())
        .collect()
}

fn expected_metadata(objects: &[(ContentRef, u64)]) -> Value {
    json!(objects
        .iter()
        .map(|(reference, size)| json!({
            "content_ref": reference.as_str(),
            "size": size,
            "media_type": null,
        }))
        .collect::<Vec<_>>())
}

fn assert_metadata(message: &Value, objects: &[(ContentRef, u64)]) {
    assert_eq!(
        message.get("attachments"),
        Some(&expected_metadata(objects)),
        "attachment results contain only ordered refs, sizes and media types"
    );
}

fn full_id(value: &Value, field: &str) -> Uuid {
    let raw = value[field].as_str().expect("canonical message identifier");
    let id = Uuid::parse_str(raw).expect("full message UUID");
    assert_eq!(id.to_string(), raw);
    id
}

async fn count(runtime: &KhiveRuntime, table: &str) -> i64 {
    let sql = match table {
        "notes" => "SELECT COUNT(*) FROM notes",
        "attachments" => "SELECT COUNT(*) FROM attachments",
        _ => panic!("unknown attachment fixture population"),
    };
    let value = runtime
        .sql()
        .reader()
        .await
        .expect("population reader")
        .query_scalar(SqlStatement {
            sql: sql.into(),
            params: vec![],
            label: Some("comm-attachment-replay-population".into()),
        })
        .await
        .expect("population query");
    match value {
        Some(SqlValue::Integer(value)) => value,
        other => panic!("population must be an integer: {other:?}"),
    }
}

async fn population(runtime: &KhiveRuntime) -> Population {
    Population {
        notes: count(runtime, "notes").await,
        attachments: count(runtime, "attachments").await,
    }
}

async fn assert_copy_rows(fixture: &Fixture, receipt: &Value, objects: &[(ContentRef, u64)]) {
    let store = fixture
        .runtime
        .attachments()
        .expect("main attachment store");
    for field in ["full_id", "recipient_id"] {
        let rows = store
            .list_attachments(full_id(receipt, field))
            .await
            .expect("message copy attachment rows");
        assert_eq!(rows.len(), objects.len(), "every copy owns all refs");
        for (index, (row, (reference, size))) in rows.iter().zip(objects).enumerate() {
            assert_eq!(row.substrate, AttachmentSubstrate::Note);
            assert_eq!(row.role, format!("message-attachment:{index}"));
            assert_eq!(&row.content_ref, reference);
            assert_eq!(row.size_bytes, Some(*size));
            assert_eq!(row.media_type, None);
        }
    }
}

fn assert_replayed(original: &Value, replay: &Value) {
    assert_eq!(original["replayed"], false);
    assert_eq!(replay["replayed"], true);
    for field in [
        "full_id",
        "recipient_id",
        "thread_id",
        "sent_at",
        "idempotency_key",
    ] {
        assert!(!original[field].is_null(), "original receipt has {field}");
        assert_eq!(
            replay[field], original[field],
            "stable receipt field {field}"
        );
    }
    assert_ne!(original["full_id"], original["recipient_id"]);
}

async fn reply_root(fixture: &Fixture) -> Value {
    dispatch(
        fixture,
        SENDER,
        "comm.send",
        json!({"to": RECIPIENT, "content": "reply root", "idempotency_key": "reply-root"}),
    )
    .await
}

async fn attachment_request(
    fixture: &Fixture,
    is_reply: bool,
    objects: &[(ContentRef, u64)],
) -> (&'static str, &'static str, Value) {
    if is_reply {
        let root = reply_root(fixture).await;
        (
            RECIPIENT,
            "comm.reply",
            json!({
                "id": root["recipient_id"],
                "content": "attached reply",
                "attachments": refs(objects),
                "idempotency_key": "attached-reply",
            }),
        )
    } else {
        (
            SENDER,
            "comm.send",
            json!({
                "to": RECIPIENT,
                "content": "attached send",
                "attachments": refs(objects),
                "idempotency_key": "attached-send",
            }),
        )
    }
}

#[tokio::test]
async fn keyed_send_attachment_replay_preserves_both_copies() {
    let fixture = fixture(false);
    let objects = publish(&fixture, 2).await;
    let (actor, verb, params) = attachment_request(&fixture, false, &objects).await;
    let first = dispatch(&fixture, actor, verb, params.clone()).await;
    assert_copy_rows(&fixture, &first, &objects).await;
    let before = population(&fixture.runtime).await;
    assert_eq!(
        before,
        Population {
            notes: 2,
            attachments: 4
        }
    );
    let replay = dispatch(&fixture, actor, verb, params).await;
    assert_replayed(&first, &replay);
    assert_eq!(
        population(&fixture.runtime).await,
        before,
        "replay adds no rows"
    );
    assert_copy_rows(&fixture, &replay, &objects).await;
}

#[tokio::test]
async fn keyed_reply_attachment_replay_preserves_both_copies() {
    let fixture = fixture(false);
    let objects = publish(&fixture, 2).await;
    let (actor, verb, params) = attachment_request(&fixture, true, &objects).await;
    let first = dispatch(&fixture, actor, verb, params.clone()).await;
    assert_copy_rows(&fixture, &first, &objects).await;
    let before = population(&fixture.runtime).await;
    assert_eq!(
        before,
        Population {
            notes: 4,
            attachments: 4
        }
    );
    let replay = dispatch(&fixture, actor, verb, params).await;
    assert_replayed(&first, &replay);
    assert_eq!(
        population(&fixture.runtime).await,
        before,
        "replay adds no rows"
    );
    assert_copy_rows(&fixture, &replay, &objects).await;
}

#[tokio::test]
async fn keyed_attachment_replay_refuses_missing_recipient_row() {
    for is_reply in [false, true] {
        let fixture = fixture(false);
        let objects = publish(&fixture, 2).await;
        let (actor, verb, params) = attachment_request(&fixture, is_reply, &objects).await;
        let first = dispatch(&fixture, actor, verb, params.clone()).await;
        assert_copy_rows(&fixture, &first, &objects).await;
        let outbound = full_id(&first, "full_id");
        let inbound = full_id(&first, "recipient_id");
        let store = fixture.runtime.attachments().expect("main attachments");
        assert!(store
            .delete_attachment(inbound, "message-attachment:1")
            .await
            .expect("remove recipient attachment"));
        let before = population(&fixture.runtime).await;
        let outbound_rows = store.list_attachments(outbound).await.unwrap();
        let inbound_rows = store.list_attachments(inbound).await.unwrap();
        assert_eq!(outbound_rows.len(), 2);
        assert_eq!(inbound_rows.len(), 1);
        let error = fixture
            .registry
            .dispatch_with_identity(verb, params, Some(identity(actor)))
            .await
            .expect_err("an incomplete attachment pair must conflict");
        let RuntimeError::Khive(conflict) = error.refusal_source() else {
            panic!("expected a structured key conflict, got {error:?}");
        };
        assert_eq!(conflict.kind(), khive_types::ErrorKind::Conflict);
        let details = conflict.details().expect("key conflict details");
        assert_eq!(details.get("reason"), Some("key_conflict"));
        assert_eq!(details.get("existing_id"), first["full_id"].as_str());
        assert_eq!(
            population(&fixture.runtime).await,
            before,
            "conflict adds no rows"
        );
        assert_eq!(
            store.list_attachments(outbound).await.unwrap(),
            outbound_rows
        );
        assert_eq!(store.list_attachments(inbound).await.unwrap(), inbound_rows);
        assert!(
            store
                .get_attachment(inbound, "message-attachment:1")
                .await
                .unwrap()
                .is_none(),
            "replay does not repair the missing row"
        );
    }
}

async fn assert_main_attachment_operation(is_reply: bool) {
    let fixture = fixture(false);
    let objects = publish(&fixture, 2).await;
    let (actor, verb, params) = attachment_request(&fixture, is_reply, &objects).await;
    let first = dispatch(&fixture, actor, verb, params).await;
    assert_copy_rows(&fixture, &first, &objects).await;
}

async fn assert_split_attachment_refusal(is_reply: bool) {
    assert_main_attachment_operation(is_reply).await;
    let fixture = fixture(true);
    let objects = publish(&fixture, 2).await;
    let (actor, verb, params) = attachment_request(&fixture, is_reply, &objects).await;
    let core = fixture.runtime.core();
    let before_comm = population(&fixture.runtime).await;
    let before_main = population(&core).await;
    assert_eq!(before_comm.attachments, 0);
    assert_eq!(before_comm.notes, if is_reply { 2 } else { 0 });
    assert_eq!(
        before_main,
        Population {
            notes: 0,
            attachments: 0
        }
    );
    let error = fixture
        .registry
        .dispatch_with_identity(verb, params, Some(identity(actor)))
        .await
        .expect_err("secondary comm attachments must refuse before writes");
    let RuntimeError::InvalidInput(reason) = error.refusal_source() else {
        panic!("expected invalid attachment parameters, got {error:?}");
    };
    assert!(
        reason.contains(verb),
        "refusal names the operation: {reason}"
    );
    assert!(
        reason.contains("canonical main comm backend"),
        "specific placement refusal: {reason}"
    );
    assert!(
        !reason.contains("unknown field"),
        "unsupported parameters are not placement proof"
    );
    assert_eq!(population(&fixture.runtime).await, before_comm);
    assert_eq!(population(&core).await, before_main);
}

#[tokio::test]
async fn split_comm_backend_refuses_attachment_send_before_writes() {
    assert_split_attachment_refusal(false).await;
}

#[tokio::test]
async fn split_comm_backend_refuses_attachment_reply_before_writes() {
    assert_split_attachment_refusal(true).await;
}

fn mailbox_params(box_name: &str) -> Value {
    let mut params = json!({"box": box_name});
    if box_name == "inbox" {
        params["status"] = json!("all");
    }
    params
}

async fn inbox(fixture: &Fixture, actor: &str, box_name: &str) -> Value {
    dispatch(fixture, actor, "comm.inbox", mailbox_params(box_name)).await
}

async fn assert_views(fixture: &Fixture, sent: &Value, objects: &[(ContentRef, u64)]) {
    assert_ne!(sent["full_id"], sent["recipient_id"]);
    for (actor, box_name, expected_id, direction) in [
        (SENDER, "sent", &sent["full_id"], "outbound"),
        (RECIPIENT, "inbox", &sent["recipient_id"], "inbound"),
    ] {
        let view = inbox(fixture, actor, box_name).await;
        let messages = view["messages"].as_array().expect("mailbox messages");
        assert_eq!(messages.len(), 1);
        assert_eq!(&messages[0]["full_id"], expected_id);
        assert_eq!(messages[0]["from"], SENDER);
        assert_eq!(messages[0]["to"], RECIPIENT);
        assert_eq!(messages[0]["direction"], direction);
        assert_eq!(messages[0]["properties"]["from_actor"], SENDER);
        assert_eq!(messages[0]["properties"]["to_actor"], RECIPIENT);
        if box_name == "inbox" {
            assert_eq!(messages[0]["properties"]["outbound_ref"], sent["full_id"]);
        }
        assert_metadata(&messages[0], objects);
        let mut projected_params = mailbox_params(box_name);
        projected_params["fields"] = json!(["full_id", "attachments"]);
        let projected = dispatch(fixture, actor, "comm.inbox", projected_params).await;
        assert_eq!(projected["messages"].as_array().unwrap().len(), 1);
        assert_eq!(projected["messages"][0]["full_id"], *expected_id);
        assert_metadata(&projected["messages"][0], objects);
        // Both copies are visible here; thread deduplication keeps the outbound twin.
        let full_thread = dispatch(fixture, actor, "comm.thread", json!({"id": expected_id})).await;
        assert_eq!(full_thread["thread_id"], sent["thread_id"]);
        assert_eq!(full_thread["count"], 1);
        let thread_messages = full_thread["messages"].as_array().unwrap();
        assert_eq!(thread_messages.len(), 1);
        assert_eq!(thread_messages[0]["full_id"], sent["full_id"]);
        assert_ne!(thread_messages[0]["full_id"], sent["recipient_id"]);
        assert_eq!(thread_messages[0]["from"], SENDER);
        assert_eq!(thread_messages[0]["to"], RECIPIENT);
        assert_eq!(thread_messages[0]["direction"], "outbound");
        assert_eq!(thread_messages[0]["properties"]["from_actor"], SENDER);
        assert_eq!(thread_messages[0]["properties"]["to_actor"], RECIPIENT);
        assert_metadata(&thread_messages[0], objects);
        let thread = dispatch(
            fixture,
            actor,
            "comm.thread",
            json!({"id": sent["thread_id"], "fields": ["full_id", "attachments"]}),
        )
        .await;
        assert_eq!(thread["thread_id"], sent["thread_id"]);
        assert_eq!(thread["count"], 1);
        assert_eq!(thread["messages"].as_array().unwrap().len(), 1);
        assert_eq!(thread["messages"][0]["full_id"], sent["full_id"]);
        assert_metadata(&thread["messages"][0], objects);
    }
}

#[tokio::test]
async fn eight_attachment_refs_keep_caller_order_and_ignore_other_roles() {
    let fixture = fixture(false);
    let mut objects = publish(&fixture, 8).await;
    objects.sort_by_key(|(reference, _)| std::cmp::Reverse(reference.clone()));
    let (actor, verb, params) = attachment_request(&fixture, false, &objects).await;
    let sent = dispatch(&fixture, actor, verb, params).await;
    assert_copy_rows(&fixture, &sent, &objects).await;
    assert_eq!(
        population(&fixture.runtime).await,
        Population {
            notes: 2,
            attachments: 16
        }
    );
    let stray = fixture
        .blobs
        .put(b"other role object".to_vec())
        .await
        .unwrap();
    assert_eq!(fixture.blobs.size(&stray).await.unwrap(), Some(17));
    let store = fixture.runtime.attachments().expect("main attachments");
    let stray_roles = [
        "message-attachment:8",
        "message-attachment:x",
        "message-attachment:00",
        "message-attachment:7x",
        "xmessage-attachment:0",
        "quarantine-original",
    ];
    for field in ["full_id", "recipient_id"] {
        let id = full_id(&sent, field);
        for role in stray_roles {
            store
                .upsert_attachment(Attachment::from_new(
                    id,
                    AttachmentSubstrate::Note,
                    NewAttachment {
                        role: role.into(),
                        content_ref: stray.clone(),
                        media_type: Some("application/octet-stream".into()),
                        size_bytes: Some(17),
                    },
                    1,
                ))
                .await
                .expect("seed a distinct existing attachment role");
        }
        assert_eq!(store.list_attachments(id).await.unwrap().len(), 14);
    }
    assert_views(&fixture, &sent, &objects).await;
    let read = dispatch(
        &fixture,
        RECIPIENT,
        "comm.read",
        json!({"id": sent["recipient_id"], "body": true}),
    )
    .await;
    assert_eq!(read["status"], "success");
    assert_metadata(&read, &objects);
}

fn assert_acknowledgement_only(read: &Value) {
    let object = read.as_object().expect("read acknowledgement object");
    assert_eq!(
        object.len(),
        5,
        "body=false preserves the base acknowledgement fields"
    );
    for field in ["id", "full_id", "status", "read", "properties"] {
        assert!(
            object.contains_key(field),
            "base acknowledgement has {field}"
        );
    }
    assert_eq!(read["status"], "success");
    assert_eq!(read["read"], true);
    assert_eq!(read["properties"]["read"], true);
    assert!(read.get("attachments").is_none());
    assert!(read.get("content").is_none());
}

#[tokio::test]
async fn attachment_read_body_opt_out_preserves_acknowledgement_shape() {
    let fixture = fixture(false);
    let objects = publish(&fixture, 2).await;
    let (actor, verb, params) = attachment_request(&fixture, false, &objects).await;
    let sent = dispatch(&fixture, actor, verb, params).await;
    assert_copy_rows(&fixture, &sent, &objects).await;
    let ack = dispatch(
        &fixture,
        RECIPIENT,
        "comm.read",
        json!({"id": sent["recipient_id"], "body": false}),
    )
    .await;
    assert_acknowledgement_only(&ack);
    assert_eq!(ack["full_id"], sent["recipient_id"]);
    let read = dispatch(
        &fixture,
        RECIPIENT,
        "comm.read",
        json!({"id": sent["recipient_id"], "body": true}),
    )
    .await;
    assert_eq!(read["status"], "success");
    assert_eq!(read["full_id"], ack["full_id"]);
    assert_eq!(read["content"], "attached send");
    assert_metadata(&read, &objects);
    let default_read = dispatch(
        &fixture,
        RECIPIENT,
        "comm.read",
        json!({"id": sent["recipient_id"]}),
    )
    .await;
    assert_metadata(&default_read, &objects);
}

#[tokio::test]
async fn attachment_free_messages_have_empty_metadata_arrays() {
    for explicit_empty in [false, true] {
        let fixture = fixture(false);
        let mut params = json!({
            "to": RECIPIENT,
            "content": "message without files",
            "idempotency_key": "empty-attachments",
        });
        if explicit_empty {
            params["attachments"] = json!([]);
        }
        let sent = dispatch(&fixture, SENDER, "comm.send", params).await;
        assert_views(&fixture, &sent, &[]).await;
        assert_eq!(
            population(&fixture.runtime).await,
            Population {
                notes: 2,
                attachments: 0
            }
        );
    }
}

#[tokio::test]
async fn attachment_free_read_acknowledgement_keeps_base_shape() {
    let fixture = fixture(false);
    let sent = reply_root(&fixture).await;
    let ack = dispatch(
        &fixture,
        RECIPIENT,
        "comm.read",
        json!({"id": sent["recipient_id"], "body": false}),
    )
    .await;
    assert_acknowledgement_only(&ack);
    assert_eq!(ack["full_id"], sent["recipient_id"]);
}

async fn dispatch_readable_attachment_view(
    fixture: &Fixture,
    actor: &str,
    verb: &str,
    params: Value,
) -> Value {
    let result = fixture
        .registry
        .dispatch_with_identity(verb, params, Some(identity(actor)))
        .await;
    assert!(
        result.is_ok(),
        "unreadable row must not fail {verb}: {result:?}"
    );
    result.unwrap()
}

fn assert_attachment_error(message: &Value, count: u64) {
    assert_eq!(
        message.get("attachments_error"),
        Some(&json!({"count": count, "reason": "unreadable_attachment"})),
        "unreadable attachment rows must be reported at their message"
    );
}

fn view_message<'a>(messages: &'a [Value], id: &str) -> &'a Value {
    let matching: Vec<_> = messages
        .iter()
        .filter(|message| message["full_id"] == id)
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "the expected message must appear exactly once"
    );
    matching[0]
}

async fn stored_message_read_flag(fixture: &Fixture, id: &str) -> SqlValue {
    let value = fixture.runtime.sql().reader().await.expect("read-flag reader")
        .query_scalar(SqlStatement {
            sql: "SELECT coalesce(json_extract(properties, '$.read'), 'absent') FROM notes WHERE id = ?1".into(),
            params: vec![SqlValue::Text(id.to_owned())],
            label: Some("attachment-fixture-stored-read-flag".into()),
        }).await.expect("read the flag independently of attachment enrichment");
    value.expect("the real inbound note exists")
}

async fn insert_unreadable_role(
    fixture: &Fixture,
    ids: &[Uuid],
    role: &str,
    object: &(ContentRef, u64),
) {
    let mut writer = fixture
        .runtime
        .sql()
        .writer()
        .await
        .expect("legacy-row fixture writer");
    writer
        .execute(SqlStatement {
            sql: "PRAGMA ignore_check_constraints = ON".into(),
            params: vec![],
            label: Some("attachment-fixture-enable-legacy-row".into()),
        })
        .await
        .expect("bypass role CHECK only on this private fixture writer");
    assert!(matches!(
        writer
            .query_scalar(SqlStatement {
                sql: "PRAGMA ignore_check_constraints".into(),
                params: vec![],
                label: None,
            })
            .await
            .unwrap(),
        Some(SqlValue::Integer(1))
    ));
    let inserted = writer.execute_batch(ids.iter().map(|id| SqlStatement {
        sql: "INSERT INTO attachments (record_uuid, substrate, role, content_ref, media_type, size_bytes, created_at) VALUES (?1, 'note', ?2, ?3, NULL, ?4, 0)".into(),
        params: vec![SqlValue::Text(id.to_string()), SqlValue::Text(role.into()),
            SqlValue::Text(object.0.as_str().to_owned()), SqlValue::Integer(object.1 as i64)],
        label: Some("attachment-fixture-legacy-row".into()),
    }).collect()).await;
    writer
        .execute(SqlStatement {
            sql: "PRAGMA ignore_check_constraints = OFF".into(),
            params: vec![],
            label: Some("attachment-fixture-restore-role-check".into()),
        })
        .await
        .expect("restore role CHECK before releasing the writer");
    assert!(matches!(
        writer
            .query_scalar(SqlStatement {
                sql: "PRAGMA ignore_check_constraints".into(),
                params: vec![],
                label: None,
            })
            .await
            .unwrap(),
        Some(SqlValue::Integer(0))
    ));
    assert_eq!(
        inserted.expect("seed the actual unreadable row"),
        ids.len() as u64
    );
}

async fn readable_thread_pair(fixture: &Fixture, objects: &[(ContentRef, u64)]) -> (Value, Value) {
    let first = dispatch(fixture, SENDER, "comm.send", json!({
        "to": RECIPIENT, "content": "attachment row owner", "attachments": [objects[0].0.as_str()],
        "idempotency_key": "row-owner",
    })).await;
    let second = dispatch(
        fixture,
        SENDER,
        "comm.send",
        json!({
            "to": RECIPIENT, "content": "readable sibling", "attachments": [objects[1].0.as_str()],
            "thread_id": first["thread_id"], "idempotency_key": "row-sibling",
        }),
    )
    .await;
    assert_eq!(first["thread_id"], second["thread_id"]);
    assert_copy_rows(fixture, &first, &objects[..1]).await;
    assert_copy_rows(fixture, &second, &objects[1..2]).await;
    let guard = inbox(fixture, RECIPIENT, "inbox").await;
    let messages = guard["messages"].as_array().unwrap();
    assert_eq!(
        messages.len(),
        2,
        "both actual inbound siblings exist before corruption"
    );
    assert_metadata(
        view_message(messages, first["recipient_id"].as_str().unwrap()),
        &objects[..1],
    );
    assert_metadata(
        view_message(messages, second["recipient_id"].as_str().unwrap()),
        &objects[1..2],
    );
    assert!(messages
        .iter()
        .all(|message| message.get("attachments_error").is_none()));
    (first, second)
}

async fn unreadable_role_preserves_message_views(role: &str) {
    let fixture = fixture(false);
    let objects = publish(&fixture, 2).await;
    let (first, second) = readable_thread_pair(&fixture, &objects).await;
    insert_unreadable_role(
        &fixture,
        &[full_id(&first, "full_id"), full_id(&first, "recipient_id")],
        role,
        &objects[0],
    )
    .await;
    for (actor, box_name, id_field) in [
        (RECIPIENT, "inbox", "recipient_id"),
        (SENDER, "sent", "full_id"),
    ] {
        let view = dispatch_readable_attachment_view(
            &fixture,
            actor,
            "comm.inbox",
            mailbox_params(box_name),
        )
        .await;
        let messages = view["messages"].as_array().unwrap();
        assert_eq!(
            messages.len(),
            2,
            "one unreadable row cannot hide either message"
        );
        let affected = view_message(messages, first[id_field].as_str().unwrap());
        assert_metadata(affected, &objects[..1]);
        assert_attachment_error(affected, 1);
        let clean = view_message(messages, second[id_field].as_str().unwrap());
        assert_metadata(clean, &objects[1..2]);
        assert!(clean.get("attachments_error").is_none());
    }
    let projected = dispatch_readable_attachment_view(
        &fixture,
        RECIPIENT,
        "comm.inbox",
        json!({"box": "inbox", "status": "all",
            "fields": ["full_id", "attachments", "attachments_error"]}),
    )
    .await;
    let projected_messages = projected["messages"].as_array().unwrap();
    assert_eq!(projected_messages.len(), 2);
    let affected = view_message(projected_messages, first["recipient_id"].as_str().unwrap());
    assert_eq!(affected.as_object().unwrap().len(), 3);
    assert_metadata(affected, &objects[..1]);
    assert_attachment_error(affected, 1);
    let clean = view_message(projected_messages, second["recipient_id"].as_str().unwrap());
    assert_metadata(clean, &objects[1..2]);
    assert_eq!(clean.as_object().unwrap().len(), 3);
    assert_eq!(
        clean.get("attachments_error"),
        Some(&Value::Null),
        "a requested absent field projects as null; the clean message must not carry an error marker"
    );
    let thread = dispatch_readable_attachment_view(
        &fixture,
        RECIPIENT,
        "comm.thread",
        json!({"id": first["recipient_id"]}),
    )
    .await;
    let messages = thread["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_metadata(
        view_message(messages, first["full_id"].as_str().unwrap()),
        &objects[..1],
    );
    assert_attachment_error(
        view_message(messages, first["full_id"].as_str().unwrap()),
        1,
    );
    assert_metadata(
        view_message(messages, second["full_id"].as_str().unwrap()),
        &objects[1..2],
    );
    assert!(view_message(messages, second["full_id"].as_str().unwrap())
        .get("attachments_error")
        .is_none());
    let read = dispatch_readable_attachment_view(
        &fixture,
        RECIPIENT,
        "comm.read",
        json!({"id": first["recipient_id"], "body": true}),
    )
    .await;
    assert_eq!(read["status"], "success");
    assert_eq!(read["full_id"], first["recipient_id"]);
    assert_metadata(&read, &objects[..1]);
    assert_attachment_error(&read, 1);
    assert!(matches!(
        stored_message_read_flag(&fixture, first["recipient_id"].as_str().unwrap()).await,
        SqlValue::Integer(1)
    ));
}

#[tokio::test]
async fn unreadable_c0_attachment_rows_keep_message_views_readable() {
    unreadable_role_preserves_message_views("message-attachment:0\u{1}").await;
}

#[tokio::test]
async fn unreadable_c1_attachment_rows_keep_message_views_readable() {
    unreadable_role_preserves_message_views("message-attachment:0\u{85}").await;
}

#[tokio::test]
async fn bulk_read_with_unreadable_later_row_marks_both_messages() {
    let fixture = fixture(false);
    let first = dispatch(
        &fixture,
        SENDER,
        "comm.send",
        json!({
            "to": RECIPIENT, "content": "first", "idempotency_key": "bulk-first",
        }),
    )
    .await;
    let second = dispatch(
        &fixture,
        SENDER,
        "comm.send",
        json!({
            "to": RECIPIENT, "content": "second", "idempotency_key": "bulk-second",
        }),
    )
    .await;
    let objects = publish(&fixture, 1).await;
    let first_id = first["recipient_id"].as_str().unwrap();
    let second_id = second["recipient_id"].as_str().unwrap();
    insert_unreadable_role(
        &fixture,
        &[full_id(&second, "recipient_id")],
        "message-attachment:0\u{1}",
        &objects[0],
    )
    .await;
    assert!(matches!(
        stored_message_read_flag(&fixture, first_id).await,
        SqlValue::Integer(0)
    ));
    assert!(matches!(
        stored_message_read_flag(&fixture, second_id).await,
        SqlValue::Integer(0)
    ));
    let result = fixture
        .registry
        .dispatch_with_identity(
            "comm.read",
            json!({"ids": [first_id, second_id], "body": true}),
            Some(identity(RECIPIENT)),
        )
        .await;
    assert!(
        result.is_ok(),
        "unreadable row must not fail bulk comm.read: {result:?}"
    );
    let result = result.unwrap();
    assert_eq!(result["status"], "success");
    assert_eq!(result["requested_count"], 2);
    assert_eq!(result["unique_count"], 2);
    assert_eq!(result["marked_count"], 2);
    assert_eq!(result["failed_count"], 0);
    assert_eq!(result["unknown_count"], 0);
    let messages = result["results"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    for (message, id) in messages.iter().zip([first_id, second_id]) {
        assert_eq!(message["full_id"], id);
        assert_eq!(message["status"], "success");
        assert_eq!(message["read"], true);
        assert_eq!(message["properties"]["read"], true);
        assert_metadata(message, &[]);
        assert!(matches!(
            stored_message_read_flag(&fixture, id).await,
            SqlValue::Integer(1)
        ));
    }
    assert!(messages[0].get("attachments_error").is_none());
    assert_attachment_error(&messages[1], 1);
}

async fn install_later_target_sql_failure(fixture: &Fixture, id: Uuid) {
    let mut writer = fixture
        .runtime
        .sql()
        .writer()
        .await
        .expect("private view writer");
    writer
        .execute_script(format!(
            "ALTER TABLE attachments RENAME TO fixture_attachment_rows; \
         CREATE VIEW attachments AS \
         SELECT record_uuid, substrate, role, \
             json_extract(CASE WHEN record_uuid = '{}' THEN 'not-json' \
                 ELSE '\"' || content_ref || '\"' END, '$') AS content_ref, \
             media_type, size_bytes, created_at FROM fixture_attachment_rows;",
            id
        ))
        .await
        .expect("install target-specific SQL execution failure");
}

#[tokio::test]
async fn bulk_attachment_lookup_failure_returns_before_any_mark() {
    let fixture = fixture(false);
    let objects = publish(&fixture, 2).await;
    let (first, second) = readable_thread_pair(&fixture, &objects).await;
    let first_id = first["recipient_id"].as_str().unwrap();
    let second_id = second["recipient_id"].as_str().unwrap();
    install_later_target_sql_failure(&fixture, full_id(&second, "recipient_id")).await;
    let store = fixture.runtime.attachments().unwrap();
    let first_rows = store
        .list_attachments(full_id(&first, "recipient_id"))
        .await
        .expect("the first actual attachment lookup still succeeds");
    assert_eq!(first_rows.len(), 1);
    assert_eq!(first_rows[0].content_ref, objects[0].0);
    let cursor_error = fixture.runtime.sql().reader().await.unwrap().query_all(SqlStatement {
        sql: "SELECT record_uuid, substrate, role, content_ref, media_type, size_bytes, created_at FROM attachments WHERE record_uuid = ?1 ORDER BY role ASC".into(),
        params: vec![SqlValue::Text(second_id.to_owned())],
        label: Some("attachment-fixture-step-failure".into()),
    }).await.expect_err("SQL cursor stepping must fail before attachment row decoding");
    assert!(matches!(
        &cursor_error,
        khive_storage::StorageError::Driver { .. }
    ));
    assert!(
        cursor_error.to_string().contains("malformed JSON"),
        "{cursor_error}"
    );
    assert!(matches!(
        stored_message_read_flag(&fixture, first_id).await,
        SqlValue::Integer(0)
    ));
    assert!(matches!(
        stored_message_read_flag(&fixture, second_id).await,
        SqlValue::Integer(0)
    ));
    let error = fixture
        .registry
        .dispatch_with_identity(
            "comm.read",
            json!({"ids": [first_id, second_id], "body": true}),
            Some(identity(RECIPIENT)),
        )
        .await
        .expect_err("a whole later-target lookup failure must refuse the bulk read");
    assert!(matches!(
        &error,
        RuntimeError::Storage(khive_storage::StorageError::Driver { .. })
    ));
    assert!(error.to_string().contains("malformed JSON"), "{error}");
    assert!(
        matches!(
            stored_message_read_flag(&fixture, first_id).await,
            SqlValue::Integer(0)
        ),
        "all field reads must complete before the first message is marked"
    );
    assert!(matches!(
        stored_message_read_flag(&fixture, second_id).await,
        SqlValue::Integer(0)
    ));
    let ack = dispatch_readable_attachment_view(
        &fixture,
        RECIPIENT,
        "comm.read",
        json!({"ids": [first_id, second_id], "body": false}),
    )
    .await;
    assert_eq!(ack["status"], "success");
    assert_eq!(
        ack["marked_count"], 2,
        "body=false does not query the failing attachment view"
    );
    for message in ack["results"].as_array().unwrap() {
        assert_acknowledgement_only(message);
    }
    assert!(matches!(
        stored_message_read_flag(&fixture, first_id).await,
        SqlValue::Integer(1)
    ));
    assert!(matches!(
        stored_message_read_flag(&fixture, second_id).await,
        SqlValue::Integer(1)
    ));
}

#[tokio::test]
async fn quarantined_attachment_rows_remain_visible_in_message_error_counts() {
    let fixture = fixture(false);
    let objects = publish(&fixture, 2).await;
    let (first, second) = readable_thread_pair(&fixture, &objects).await;
    let mut writer = fixture
        .runtime
        .sql()
        .writer()
        .await
        .expect("quarantine fixture writer");
    let changed = writer.execute_batch(["full_id", "recipient_id"].iter().map(|field| SqlStatement {
        sql: "INSERT INTO attachment_quarantine (record_uuid, substrate, role, content_ref, media_type, size_bytes, created_at, reason) VALUES (?1, 'note', ?2, ?3, NULL, ?4, 0, 'invalid_role')".into(),
        params: vec![SqlValue::Text(full_id(&first, field).to_string()),
            SqlValue::Text("message-attachment:0\u{1}".into()),
            SqlValue::Text(objects[0].0.as_str().to_owned()), SqlValue::Integer(objects[0].1 as i64)],
        label: Some("attachment-fixture-quarantined-role".into()),
    }).collect()).await.expect("candidate migration 047 provides the actual quarantine schema");
    assert_eq!(changed, 2);
    drop(writer);
    assert_copy_rows(&fixture, &first, &objects[..1]).await;
    for field in ["full_id", "recipient_id"] {
        let count = fixture
            .runtime
            .sql()
            .reader()
            .await
            .unwrap()
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM attachment_quarantine WHERE record_uuid = ?1".into(),
                params: vec![SqlValue::Text(full_id(&first, field).to_string())],
                label: Some("attachment-fixture-quarantine-count".into()),
            })
            .await
            .unwrap();
        assert!(matches!(count, Some(SqlValue::Integer(1))));
    }
    for (actor, box_name, id_field) in [
        (RECIPIENT, "inbox", "recipient_id"),
        (SENDER, "sent", "full_id"),
    ] {
        let view = dispatch_readable_attachment_view(
            &fixture,
            actor,
            "comm.inbox",
            mailbox_params(box_name),
        )
        .await;
        let messages = view["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        let affected = view_message(messages, first[id_field].as_str().unwrap());
        assert_metadata(affected, &objects[..1]);
        assert_attachment_error(affected, 1);
        let clean = view_message(messages, second[id_field].as_str().unwrap());
        assert_metadata(clean, &objects[1..2]);
        assert!(clean.get("attachments_error").is_none());
    }
    let thread = dispatch_readable_attachment_view(
        &fixture,
        RECIPIENT,
        "comm.thread",
        json!({"id": first["recipient_id"]}),
    )
    .await;
    let messages = thread["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_metadata(
        view_message(messages, first["full_id"].as_str().unwrap()),
        &objects[..1],
    );
    assert_attachment_error(
        view_message(messages, first["full_id"].as_str().unwrap()),
        1,
    );
    assert!(view_message(messages, second["full_id"].as_str().unwrap())
        .get("attachments_error")
        .is_none());
    let read = dispatch_readable_attachment_view(
        &fixture,
        RECIPIENT,
        "comm.read",
        json!({"id": first["recipient_id"], "body": true}),
    )
    .await;
    assert_eq!(read["status"], "success");
    assert_metadata(&read, &objects[..1]);
    assert_attachment_error(&read, 1);
    assert!(matches!(
        stored_message_read_flag(&fixture, first["recipient_id"].as_str().unwrap()).await,
        SqlValue::Integer(1)
    ));
}

async fn replay_row_snapshot(fixture: &Fixture, include_quarantine: bool) -> Value {
    let mut reader = fixture
        .runtime
        .sql()
        .reader()
        .await
        .expect("replay state reader");
    let mut snapshots = Vec::new();
    for sql in [
        "SELECT * FROM notes ORDER BY id",
        "SELECT * FROM attachments ORDER BY record_uuid COLLATE BINARY, role COLLATE BINARY",
    ] {
        let rows = reader
            .query_all(SqlStatement {
                sql: sql.into(),
                params: vec![],
                label: Some("keyed-replay-integrity-snapshot".into()),
            })
            .await
            .expect("replay row snapshot");
        snapshots.push(serde_json::to_value(rows).expect("snapshot native column values"));
    }
    if include_quarantine {
        let rows = reader
            .query_all(SqlStatement {
                sql: "SELECT * FROM attachment_quarantine ORDER BY record_uuid COLLATE BINARY, role COLLATE BINARY".into(),
                params: vec![], label: Some("keyed-replay-quarantine-snapshot".into()),
            })
            .await
            .expect("actual quarantine row snapshot");
        snapshots.push(serde_json::to_value(rows).expect("quarantine column values"));
    }
    json!(snapshots)
}

async fn keyed_replay_refuses_unreadable_copy(quarantined: bool) {
    for is_reply in [false, true] {
        for copy_field in ["full_id", "recipient_id"] {
            for attachment_count in [0, 2] {
                let fixture = fixture(false);
                let objects = publish(&fixture, attachment_count.max(1)).await;
                let expected = &objects[..attachment_count];
                let (actor, verb, params) = attachment_request(&fixture, is_reply, expected).await;
                let first = dispatch(&fixture, actor, verb, params.clone()).await;
                assert_copy_rows(&fixture, &first, expected).await;
                let clean_population = population(&fixture.runtime).await;
                let clean_replay = dispatch(&fixture, actor, verb, params.clone()).await;
                assert_replayed(&first, &clean_replay);
                assert_eq!(population(&fixture.runtime).await, clean_population);
                let owner = full_id(&first, copy_field);
                if quarantined {
                    let mut writer = fixture
                        .runtime
                        .sql()
                        .writer()
                        .await
                        .expect("quarantine fixture writer");
                    assert_eq!(writer.execute(SqlStatement {
                        sql: "INSERT INTO attachment_quarantine (record_uuid, substrate, role, content_ref, media_type, size_bytes, created_at, reason) VALUES (?1, 'note', ?2, ?3, NULL, ?4, 0, 'invalid_role')".into(),
                        params: vec![SqlValue::Text(owner.to_string()), SqlValue::Text("message-attachment:0\u{85}".into()),
                            SqlValue::Text(objects[0].0.as_str().to_owned()), SqlValue::Integer(objects[0].1 as i64)],
                        label: Some("keyed-replay-retained-quarantine-row".into()),
                    }).await.expect("actual quarantine schema"), 1);
                    let rows = fixture
                        .runtime
                        .attachments()
                        .unwrap()
                        .list_attachments(owner)
                        .await
                        .expect("canonical rows remain readable");
                    assert_eq!(rows.len(), attachment_count);
                } else {
                    insert_unreadable_role(
                        &fixture,
                        &[owner],
                        "message-attachment:0\u{85}",
                        &objects[0],
                    )
                    .await;
                    assert!(
                        fixture
                            .runtime
                            .attachments()
                            .unwrap()
                            .list_attachments(owner)
                            .await
                            .is_err(),
                        "the inserted row must be unreadable through the unchanged strict API"
                    );
                }
                let before = replay_row_snapshot(&fixture, quarantined).await;
                let before_population = population(&fixture.runtime).await;
                let result = fixture
                    .registry
                    .dispatch_with_identity(verb, params, Some(identity(actor)))
                    .await;
                assert!(result.is_err(), "a keyed replay must refuse an unreadable {copy_field} copy, including an attachment-free request: {result:?}");
                let error = result.unwrap_err();
                let RuntimeError::Khive(conflict) = error.refusal_source() else {
                    panic!("unreadable keyed pair must return key_conflict, got {error:?}");
                };
                assert_eq!(conflict.kind(), khive_types::ErrorKind::Conflict);
                let details = conflict.details().expect("key conflict details");
                assert_eq!(details.get("reason"), Some("key_conflict"));
                assert_eq!(details.get("existing_id"), first["full_id"].as_str());
                assert_eq!(
                    population(&fixture.runtime).await,
                    before_population,
                    "refusal must add no notes or attachments"
                );
                assert_eq!(
                    replay_row_snapshot(&fixture, quarantined).await,
                    before,
                    "refusal must preserve every note, attachment and retained quarantine value"
                );
            }
        }
    }
}

#[tokio::test]
async fn keyed_attachment_replay_refuses_unreadable_rows_on_either_copy() {
    keyed_replay_refuses_unreadable_copy(false).await;
}

#[tokio::test]
async fn keyed_attachment_replay_refuses_quarantined_rows_on_either_copy() {
    keyed_replay_refuses_unreadable_copy(true).await;
}

async fn attachment_replay_publication_snapshot(fixture: &Fixture) -> Value {
    let rows = replay_row_snapshot(fixture, false).await;
    let mut reader = fixture
        .runtime
        .sql()
        .reader()
        .await
        .expect("replay event snapshot reader");
    let mut events = Vec::new();
    for sql in [
        "SELECT * FROM events ORDER BY id",
        "SELECT * FROM event_observations ORDER BY event_id, role, position",
    ] {
        events.push(
            serde_json::to_value(
                reader
                    .query_all(SqlStatement {
                        sql: sql.into(),
                        params: vec![],
                        label: Some("attachment-replay-publication-snapshot".into()),
                    })
                    .await
                    .expect("replay event snapshot query"),
            )
            .expect("snapshot SQL column values"),
        );
    }
    json!({"notes_and_attachments": rows, "events_and_observations": events})
}

#[tokio::test]
async fn attachment_free_key_replays_pre_attachment_request_shape() {
    let fixture = fixture(false);
    let params = json!({
        "to": RECIPIENT, "content": "message from before file attachments",
        "subject": "Legacy key", "tags": ["old-shape"], "idempotency_key": "legacy-empty-files",
    });
    let first = dispatch(&fixture, SENDER, "comm.send", params.clone()).await;
    assert_eq!(
        population(&fixture.runtime).await,
        Population {
            notes: 2,
            attachments: 0
        }
    );
    assert_copy_rows(&fixture, &first, &[]).await;
    // Install the literal historical request on a real, intact public-send
    // pair. This stays independent of today's identity constructor, including
    // if it starts adding an empty attachments member to newly minted keys.
    let historical = json!({
        "version": 1, "op": "send", "to": RECIPIENT,
        "content": "message from before file attachments", "subject": "Legacy key",
        "thread_id": null, "tags": ["old-shape"], "reply_parent_id": null,
    });
    assert!(historical.get("attachments").is_none());
    let outbound = full_id(&first, "full_id");
    let changed = fixture.runtime.sql().writer().await.expect("historical identity writer")
        .execute(SqlStatement {
            sql: "UPDATE notes SET properties=json_set(properties, '$.idempotency_request', json(?1)) \
                  WHERE id=?2 AND namespace=?3 AND kind='message' AND key IS NOT NULL".into(),
            params: vec![SqlValue::Text(historical.to_string()), SqlValue::Text(outbound.to_string()), SqlValue::Text(NAMESPACE.into())],
            label: Some("install-pre-attachment-message-request".into()),
        }).await.expect("store the pre-attachment request shape");
    assert_eq!(changed, 1, "only the held outbound identity is installed");
    let token = fixture.runtime.authorize(Namespace::local()).unwrap();
    let stored = fixture
        .runtime
        .notes(&token)
        .unwrap()
        .get_note(outbound)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.properties.as_ref().unwrap()["idempotency_request"],
        historical
    );
    let physical_key = format!(
        "comm-v1:{}",
        json!([NAMESPACE, SENDER, "legacy-empty-files"])
    );
    assert_eq!(stored.key.as_deref(), Some(physical_key.as_str()));
    let before = attachment_replay_publication_snapshot(&fixture).await;
    for explicit_empty in [false, true] {
        let mut replay_params = params.clone();
        if explicit_empty {
            replay_params["attachments"] = json!([]);
        }
        let result = fixture
            .registry
            .dispatch_with_identity("comm.send", replay_params, Some(identity(SENDER)))
            .await;
        assert!(
            result.is_ok(),
            "omitted and empty files must replay the stored historical identity: {result:?}"
        );
        let replay = result.unwrap();
        assert_replayed(&first, &replay);
        assert_copy_rows(&fixture, &replay, &[]).await;
        assert_eq!(attachment_replay_publication_snapshot(&fixture).await, before,
            "legacy replay must not modify either note, its key, attachments, events or projections");
    }
}

#[tokio::test]
async fn keyed_attachment_replay_refuses_different_list_and_order() {
    for is_reply in [false, true] {
        let fixture = fixture(false);
        let objects = publish(&fixture, 3).await;
        let (actor, verb, params) = attachment_request(&fixture, is_reply, &objects[..2]).await;
        let first = dispatch(&fixture, actor, verb, params.clone()).await;
        assert_copy_rows(&fixture, &first, &objects[..2]).await;
        let intact = dispatch(&fixture, actor, verb, params.clone()).await;
        assert_replayed(&first, &intact);
        let before = attachment_replay_publication_snapshot(&fixture).await;
        for changed_refs in [
            vec![objects[0].0.as_str(), objects[2].0.as_str()],
            vec![objects[1].0.as_str(), objects[0].0.as_str()],
        ] {
            let mut changed_request = params.clone();
            changed_request["attachments"] = json!(changed_refs);
            let result = fixture
                .registry
                .dispatch_with_identity(verb, changed_request, Some(identity(actor)))
                .await;
            assert!(
                result.is_err(),
                "a different valid ref list or order must conflict: {result:?}"
            );
            let error = result.unwrap_err();
            let RuntimeError::Khive(conflict) = error.refusal_source() else {
                panic!("expected an intact-pair key conflict, got {error:?}");
            };
            assert_eq!(conflict.kind(), khive_types::ErrorKind::Conflict);
            let details = conflict.details().expect("key conflict details");
            assert_eq!(details.get("reason"), Some("key_conflict"));
            assert_eq!(details.get("existing_id"), first["full_id"].as_str());
            assert_eq!(
                attachment_replay_publication_snapshot(&fixture).await,
                before,
                "conflict must not repair, reorder or replace either copy or publish any rows"
            );
            assert_copy_rows(&fixture, &first, &objects[..2]).await;
        }
    }
}

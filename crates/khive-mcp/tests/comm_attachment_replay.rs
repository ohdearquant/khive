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

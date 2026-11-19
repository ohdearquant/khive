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

#[path = "comm_attachment_replay/lifecycle_tests.rs"]
mod lifecycle_tests;

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

include!("comm_attachment_replay_tests/replay_and_view_tests.rs");

include!("comm_attachment_replay_tests/acknowledgement_shape_tests.rs");

include!("comm_attachment_replay_tests/unreadable_attachment_view_tests.rs");

include!("comm_attachment_replay_tests/lookup_and_quarantine_tests.rs");

include!("comm_attachment_replay_tests/unreadable_replay_tests.rs");

include!("comm_attachment_replay_tests/publication_and_conflict_tests.rs");

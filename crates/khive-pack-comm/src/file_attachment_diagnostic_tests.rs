use std::sync::Arc;

use khive_runtime::{
    KhiveRuntime, Namespace, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::note::{FilterOp, NoteFilter, PropertyFilter};
use khive_storage::types::{PageRequest, SqlValue};
use khive_storage::{BlobStore as _, ContentRef};
use serde_json::json;
use uuid::Uuid;

struct Fixture {
    runtime: KhiveRuntime,
    registry: VerbRegistry,
    first: ContentRef,
    second: ContentRef,
    _blob_root: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            default_namespace: Namespace::local(),
            actor_id: Some("actor:sender".into()),
            brain_profile: None,
            wal_ceiling_env_raw: None,
            events_split: None,
            packs: vec!["kg".into(), "comm".into()],
            ..RuntimeConfig::no_embeddings()
        })
        .expect("in-memory runtime with migrated attachment storage");
        runtime.attachments().expect("canonical attachment store");
        let blob_root = tempfile::tempdir().expect("isolated blob directory");
        let store = Arc::new(
            khive_db::stores::blob::FsBlobStore::new(blob_root.path().to_path_buf(), 0)
                .expect("blob store"),
        );
        let first = store.put(b"first".to_vec()).await.expect("first object");
        let second = store
            .put(b"second object".to_vec())
            .await
            .expect("second object");
        runtime
            .install_blob_store(store)
            .expect("install blob store");
        let mut builder = VerbRegistryBuilder::new();
        builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
        builder.register(crate::CommPack::new(runtime.clone()));
        builder.with_actor_id(Some("actor:sender".into()));
        builder.with_default_namespace("local");
        Self {
            runtime,
            registry: builder.build().expect("comm registry"),
            first,
            second,
            _blob_root: blob_root,
        }
    }

    async fn invalid_send(&self, raw: &str) -> String {
        let error = self
            .registry
            .dispatch(
                "comm.send",
                json!({"to": "actor:recipient", "content": "attachment", "attachments": [raw]}),
            )
            .await
            .expect_err("malformed reference must refuse the send");
        invalid_input(error)
    }
}

fn invalid_input(error: RuntimeError) -> String {
    match error {
        RuntimeError::InvalidInput(message) => message,
        other => panic!("expected an invalid attachment input error, got {other:?}"),
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn attachment_short_ascii_and_invalid_hex_are_echoed_once() {
    let fixture = Fixture::new().await;
    for raw in [
        String::new(),
        "tiny-invalid".to_string(),
        "A".repeat(64),
        "g".repeat(64),
    ] {
        let message = fixture.invalid_send(&raw).await;
        let quoted = format!("{raw:?}");
        assert_eq!(
            message.matches(quoted.as_str()).count(),
            1,
            "one input preview"
        );
        assert_eq!(
            message,
            format!(
                "comm.send: invalid attachment {raw:?}: expected 64 lowercase hex characters (0-9, a-f), got {} bytes",
                raw.len()
            )
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn attachment_ascii_preview_is_bounded_at_64_bytes() {
    let fixture = Fixture::new().await;
    for (raw, expected_preview, suffix) in [
        ("z".repeat(63), "z".repeat(63), ""),
        ("z".repeat(64), "z".repeat(64), ""),
        ("z".repeat(65), "z".repeat(64), "..."),
        (
            format!("{}tail-marker", "z".repeat(8192)),
            "z".repeat(64),
            "...",
        ),
    ] {
        let message = fixture.invalid_send(&raw).await;
        assert!(message.len() <= 512, "bounded diagnostic byte length");
        assert!(message.starts_with(&format!(
            "comm.send: invalid attachment {expected_preview:?}{suffix}:"
        )));
        assert!(!message.contains("tail-marker"));
        assert!(message.ends_with(&format!("got {} bytes", raw.len())));
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn attachment_unicode_preview_ends_on_complete_characters() {
    let fixture = Fixture::new().await;
    for (raw, expected_preview, suffix) in [
        ("界".repeat(4096), "界".repeat(21), "..."),
        ("🙂".repeat(4096), "🙂".repeat(16), "..."),
        (format!("{}界suffix", "x".repeat(63)), "x".repeat(63), "..."),
        (
            format!("{}z", "界".repeat(21)),
            format!("{}z", "界".repeat(21)),
            "",
        ),
    ] {
        let message = fixture.invalid_send(&raw).await;
        assert!(message.len() <= 512, "Unicode diagnostic byte length");
        assert!(message.starts_with(&format!(
            "comm.send: invalid attachment {expected_preview:?}{suffix}:"
        )));
        assert_eq!(message.matches(expected_preview.as_str()).count(), 1);
        assert!(message.ends_with(&format!("got {} bytes", raw.len())));
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn attachment_escaped_preview_is_single_line_and_bounded() {
    let fixture = Fixture::new().await;
    let raw = format!("{}tail-marker", "\u{1b}\n".repeat(4096));
    let message = fixture.invalid_send(&raw).await;
    assert!(message.len() <= 512, "escaping must also remain bounded");
    assert!(message.starts_with(&format!(
        "comm.send: invalid attachment \"{}\"...:",
        "\\u{1b}\\n".repeat(32)
    )));
    assert!(!message.contains('\u{1b}'));
    assert!(!message.contains('\n'));
    assert!(!message.contains("tail-marker"));
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn attachment_reply_uses_the_same_bounded_input_diagnostic() {
    let fixture = Fixture::new().await;
    let original = fixture
        .registry
        .dispatch(
            "comm.send",
            json!({"to": "actor:recipient", "content": "original message"}),
        )
        .await
        .expect("valid message to reply to");
    let raw = format!("{}tail-marker", "界".repeat(4096));
    let error = fixture
        .registry
        .dispatch(
            "comm.reply",
            json!({"id": original["full_id"], "content": "reply", "attachments": [raw]}),
        )
        .await
        .expect_err("malformed reply attachment must refuse");
    let message = invalid_input(error);
    assert!(message.len() <= 512);
    assert!(message.starts_with(&format!(
        "comm.reply: invalid attachment {:?}...:",
        "界".repeat(21)
    )));
    assert!(!message.contains("tail-marker"));
    let token = fixture
        .runtime
        .authorize(Namespace::local())
        .expect("local token");
    assert_eq!(
        fixture
            .runtime
            .notes(&token)
            .expect("notes")
            .count_notes("local", Some("message"))
            .await
            .expect("message count"),
        2,
        "a refused reply must leave only the original message pair"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn attachment_valid_references_preserve_order_size_and_duplicate_refusal() {
    let fixture = Fixture::new().await;
    for references in [
        vec![fixture.first.to_string(), fixture.second.to_string()],
        vec![fixture.second.to_string(), fixture.first.to_string()],
    ] {
        let attachments = super::prepare(
            &fixture.runtime,
            "comm.send",
            "actor:recipient",
            &references,
        )
        .await
        .expect("valid stored references");
        assert_eq!(attachments.len(), 2);
        for (index, attachment) in attachments.iter().enumerate() {
            assert_eq!(attachment.content_ref.as_str(), references[index].as_str());
            assert_eq!(attachment.role, format!("message-attachment:{index}"));
            assert_eq!(attachment.media_type, None);
            assert_eq!(
                attachment.size_bytes,
                Some(if attachment.content_ref == fixture.first {
                    5
                } else {
                    13
                })
            );
        }
    }
    let receipt = fixture
        .registry
        .dispatch(
            "comm.send",
            json!({
                "to": "actor:recipient", "content": "stored attachments",
                "attachments": [fixture.second.to_string(), fixture.first.to_string()],
            }),
        )
        .await
        .expect("valid attachment-bearing send");
    let outbound_id = receipt["full_id"]
        .as_str()
        .expect("outbound message UUID")
        .parse::<Uuid>()
        .expect("canonical outbound UUID");
    let token = fixture
        .runtime
        .authorize(Namespace::local())
        .expect("local token");
    let inbound = fixture
        .runtime
        .notes(&token)
        .expect("notes")
        .query_notes_filtered_count_free(
            "local",
            &NoteFilter {
                kind: Some("message".into()),
                property_filters: vec![
                    PropertyFilter {
                        json_path: "$.direction".into(),
                        op: FilterOp::Eq,
                        value: SqlValue::Text("inbound".into()),
                    },
                    PropertyFilter {
                        json_path: "$.outbound_ref".into(),
                        op: FilterOp::Eq,
                        value: SqlValue::Text(outbound_id.to_string()),
                    },
                ],
                ..Default::default()
            },
            PageRequest {
                limit: 2,
                offset: 0,
            },
        )
        .await
        .expect("stored inbound sibling lookup");
    assert_eq!(inbound.items.len(), 1, "one stored inbound sibling");
    let inbound_id = inbound.items[0].id;
    assert_ne!(inbound_id, outbound_id, "distinct message copies");
    for id in [outbound_id, inbound_id] {
        let report = super::rows(&fixture.runtime, id)
            .await
            .expect("stored message attachments");
        assert_eq!(report.unreadable_count, 0);
        assert_eq!(report.attachments.len(), 2);
        assert_eq!(report.attachments[0].content_ref, fixture.second);
        assert_eq!(report.attachments[0].size_bytes, Some(13));
        assert_eq!(report.attachments[1].content_ref, fixture.first);
        assert_eq!(report.attachments[1].size_bytes, Some(5));
    }
    let duplicate = fixture.first.to_string();
    let error = super::prepare(
        &fixture.runtime,
        "comm.send",
        "actor:recipient",
        &[duplicate.clone(), duplicate.clone()],
    )
    .await
    .expect_err("valid repeated reference remains a duplicate refusal");
    assert_eq!(
        invalid_input(error),
        format!("comm.send: duplicate attachment {duplicate}")
    );
}

use khive_pack_comm as _;
use khive_pack_kg as _;
use khive_runtime::{KhiveRuntime, Namespace, PackRegistry, VerbRegistry, VerbRegistryBuilder};
use khive_storage::{Attachment, BlobStore as _, ContentRef, Note};
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

const EXTERNAL_ID: &str = "imap:mail.example.com:first@example.com:17:42";
const LEGACY_ID: &str = "imap:mail.example.com:17:42";
const FIRST_SLUG: &str = "first@example.com";
const SECOND_SLUG: &str = "second@example.com";
const ORIGINAL_BYTES: &[u8] = b"retained quarantine original";

struct Fixture {
    runtime: KhiveRuntime,
    registry: VerbRegistry,
    blobs: Arc<khive_db::stores::blob::FsBlobStore>,
    original_ref: ContentRef,
    _blob_root: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        let blob_root = tempfile::tempdir().expect("blob root");
        let blobs = Arc::new(
            khive_db::stores::blob::FsBlobStore::new(blob_root.path().to_path_buf(), 0)
                .expect("blob store"),
        );
        let original_ref = blobs
            .put(ORIGINAL_BYTES.to_vec())
            .await
            .expect("publish original");
        runtime
            .install_blob_store(blobs.clone())
            .expect("install blob store");
        let mut builder = VerbRegistryBuilder::new();
        PackRegistry::register_packs(
            &["kg".to_string(), "comm".to_string()],
            runtime.clone(),
            &mut builder,
        )
        .expect("register packs with the channel-ingest grant");
        builder.with_default_namespace("local");
        let registry = builder.build().expect("build registry");
        Self {
            runtime,
            registry,
            blobs,
            original_ref,
            _blob_root: blob_root,
        }
    }

    fn original_delivery(&self) -> Value {
        json!({
            "from": "email:original@example.com",
            "to": "email:maintainer@example.com",
            "content": "stored quarantine body",
            "subject": "stored subject",
            "thread_id": "00000000-0000-4000-8000-000000000023",
            "sent_at": "2026-10-17T00:00:00Z",
            "channel_kind": "email",
            "channel_slug": FIRST_SLUG,
            "external_id": EXTERNAL_ID,
            "metadata": {
                "quarantined": true,
                "quarantine_content_ref": self.original_ref.to_string(),
                "replay_marker": {"phase": "stored"},
            },
        })
    }

    async fn note(&self, id: Uuid) -> Note {
        let token = self.runtime.authorize(Namespace::local()).expect("token");
        self.runtime
            .notes(&token)
            .expect("notes")
            .get_note(id)
            .await
            .expect("note lookup")
            .expect("stored note")
    }

    async fn snapshot(&self, id: Uuid) -> (Note, Vec<Attachment>, Vec<u8>) {
        let note = self.note(id).await;
        let attachments = self
            .runtime
            .core()
            .attachments()
            .expect("attachments")
            .list_attachments(id)
            .await
            .expect("attachment rows");
        let original = self
            .blobs
            .get_bounded_verified(&self.original_ref, 1024)
            .await
            .expect("retained original bytes");
        (note, attachments, original)
    }

    async fn message_count(&self) -> u64 {
        let token = self.runtime.authorize(Namespace::local()).expect("token");
        self.runtime
            .notes(&token)
            .expect("notes")
            .count_notes("local", Some("message"))
            .await
            .expect("message count")
    }
}

fn replay(slug: &str, with_legacy_lookup: bool) -> Value {
    let mut input = json!({
        "from": "email:changed@example.com",
        "to": "email:different-recipient@example.com",
        "content": "changed replay body",
        "subject": "changed subject",
        "thread_id": "00000000-0000-4000-8000-000000000024",
        "sent_at": "2026-10-18T00:00:00Z",
        "channel_kind": "email",
        "channel_slug": slug,
        "external_id": EXTERNAL_ID,
        "metadata": {
            "quarantined": true,
            "replay_marker": {"phase": "changed"},
        },
    });
    if with_legacy_lookup {
        input["legacy_external_id"] = json!(LEGACY_ID);
    }
    input
}

fn note_id(ack: &Value) -> Uuid {
    ack["full_id"]
        .as_str()
        .expect("new message receipt includes full_id")
        .parse()
        .expect("canonical note UUID")
}

#[tokio::test]
async fn attachment_free_same_channel_duplicate_ack_preserves_stored_quarantine() {
    // Cover both the account-key prelookup and conditional-insert duplicate route.
    for with_legacy_lookup in [true, false] {
        let f = Fixture::new().await;
        let stored = f
            .registry
            .dispatch("comm.ingest", f.original_delivery())
            .await
            .expect("seed retained quarantine");
        assert_eq!(stored["deduplicated"], false);
        assert_eq!(stored["quarantine_original_retained"], true);
        let id = note_id(&stored);
        let before = f.snapshot(id).await;
        assert_eq!(before.1.len(), 1);
        assert_eq!(before.1[0].role, "quarantine-original");
        assert_eq!(before.1[0].content_ref, f.original_ref);
        assert_eq!(before.2.as_slice(), ORIGINAL_BYTES);
        assert!(before.0.expires_at.is_some());

        let input = replay(FIRST_SLUG, with_legacy_lookup);
        let properties = before.0.properties.as_ref().expect("stored properties");
        assert_ne!(input["content"].as_str(), Some(before.0.content.as_str()));
        assert_ne!(input["from"], properties["from"]);
        assert_ne!(input["thread_id"], properties["thread_id"]);
        assert_ne!(
            input["metadata"]["replay_marker"],
            properties["replay_marker"]
        );
        assert!(input["metadata"].get("quarantine_content_ref").is_none());
        let ack = f
            .registry
            .dispatch("comm.ingest", input)
            .await
            .expect("attachment-free same-channel duplicate is acknowledged");
        assert_eq!(ack["ok"], true);
        assert_eq!(ack["deduplicated"], true);
        assert_eq!(ack["external_id"], EXTERNAL_ID);
        assert_eq!(ack["thread_id"], properties["thread_id"]);
        assert_eq!(f.snapshot(id).await, before);
        assert_eq!(f.message_count().await, 1);
    }
}

#[tokio::test]
async fn attachment_free_duplicate_identity_is_scoped_to_channel_slug() {
    for with_legacy_lookup in [true, false] {
        let f = Fixture::new().await;
        // Prove the other slug and attachment-free input are valid independently
        // of the colliding external ID; an input-validation refusal is no oracle.
        let mut valid_other_channel = replay(SECOND_SLUG, with_legacy_lookup);
        valid_other_channel["external_id"] = json!("imap:mail.example.com:scope-control:17:42");
        let probe = f
            .registry
            .dispatch("comm.ingest", valid_other_channel)
            .await
            .expect("other channel accepts this delivery shape");
        assert_eq!(probe["deduplicated"], false);
        let probe_id = note_id(&probe);
        let probe_before = f.note(probe_id).await;
        assert_eq!(
            probe_before.properties.as_ref().unwrap()["channel_slug"],
            SECOND_SLUG
        );

        let first = f
            .registry
            .dispatch("comm.ingest", f.original_delivery())
            .await
            .expect("first channel's quarantine");
        let first_id = note_id(&first);
        let first_before = f.snapshot(first_id).await;
        assert_eq!(f.message_count().await, 2);

        let input = replay(SECOND_SLUG, with_legacy_lookup);
        let second = f
            .registry
            .dispatch("comm.ingest", input.clone())
            .await
            .expect("same external ID on another valid channel is a new delivery");
        assert_eq!(second["deduplicated"], false);
        assert_eq!(second["quarantine_original_retained"], false);
        let second_id = note_id(&second);
        assert_ne!(second_id, first_id);
        assert_ne!(second_id, probe_id);
        let second_before = f.snapshot(second_id).await;
        let properties = second_before
            .0
            .properties
            .as_ref()
            .expect("second properties");
        assert_eq!(
            second_before.0.content.as_str(),
            input["content"].as_str().unwrap()
        );
        assert_eq!(properties["external_id"], EXTERNAL_ID);
        assert_eq!(properties["channel_kind"], "email");
        assert_eq!(properties["channel_slug"], SECOND_SLUG);
        assert_eq!(properties["from"], input["from"]);
        assert_eq!(
            properties["replay_marker"],
            input["metadata"]["replay_marker"]
        );
        assert!(properties.get("quarantine_content_ref").is_none());
        assert!(second_before.1.is_empty());
        assert_eq!(f.snapshot(first_id).await, first_before);
        assert_eq!(f.note(probe_id).await, probe_before);
        assert_eq!(f.message_count().await, 3);

        let retry = f
            .registry
            .dispatch("comm.ingest", input)
            .await
            .expect("second channel's own duplicate");
        assert_eq!(retry["deduplicated"], true);
        assert_eq!(retry["thread_id"], second["thread_id"]);
        assert_eq!(f.snapshot(second_id).await, second_before);
        assert_eq!(f.snapshot(first_id).await, first_before);
        assert_eq!(f.note(probe_id).await, probe_before);
        assert_eq!(f.message_count().await, 3);
    }
}

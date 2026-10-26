// Copyright 2026 Haiyang Li. Licensed under Apache-2.0.
//
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use khive_db::stores::blob::FsBlobStore;
use khive_pack_comm::CommPack;
use khive_runtime::operations::QuarantineOriginalRetention;
use khive_runtime::{
    BackendId, ChannelIngestCapability, KhiveRuntime, Namespace, RuntimeConfig, VerbRegistry,
    VerbRegistryBuilder,
};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::{
    Attachment, AttachmentSubstrate, BlobStore, ContentRef, NewAttachment, Note, StorageError,
};
use serde_json::{json, Value};
use uuid::Uuid;

const RETENTION: Duration = Duration::from_secs(60);
const ORIGINAL: &[u8] = b"byte-exact quarantined original";

struct Fixture {
    _root: tempfile::TempDir,
    runtime: KhiveRuntime,
    registry: VerbRegistry,
    blobs: Arc<FsBlobStore>,
    namespace: String,
}

impl Fixture {
    fn new(secondary: bool) -> Self {
        let main = Arc::new(khive_db::StorageBackend::memory().unwrap());
        main.pool().run_migrations().unwrap();
        let backend = if secondary {
            let backend = Arc::new(khive_db::StorageBackend::memory().unwrap());
            backend.pool().run_migrations().unwrap();
            backend
        } else {
            main.clone()
        };
        let config = RuntimeConfig {
            backend_id: if secondary {
                BackendId::parse("comm-secondary").unwrap()
            } else {
                BackendId::main()
            },
            ..RuntimeConfig::no_embeddings()
        };
        let mut runtime = KhiveRuntime::from_backend(backend, config);
        if secondary {
            runtime = runtime.with_core_backend(main);
        }
        let root = tempfile::tempdir().unwrap();
        let blobs = Arc::new(
            FsBlobStore::new(root.path().join("blobs"), 0)
                .unwrap()
                .with_orphan_sweep_grace(RETENTION),
        );
        runtime.install_blob_store(blobs.clone()).unwrap();
        let namespace = format!("quarantine-{}", Uuid::new_v4().simple());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(khive_pack_kg::KgPack::new(runtime.core()));
        builder.register(
            CommPack::new_with_channel_ingest_capability(
                runtime.clone(),
                ChannelIngestCapability::grant_for_direct_composition(),
            )
            .with_quarantine_retention(RETENTION),
        );
        builder.with_default_namespace(&namespace);
        let registry = builder.build().unwrap();
        Self {
            _root: root,
            runtime,
            registry,
            blobs,
            namespace,
        }
    }

    fn params(&self, external_id: &str, original: Option<&ContentRef>) -> Value {
        let mut metadata = json!({"quarantined": true, "quarantine_reason": "off-allowlist"});
        if let Some(original) = original {
            metadata["quarantine_content_ref"] = json!(original.to_string());
        }
        json!({
            "from": "external:sender", "to": self.namespace,
            "content": "quarantined inbound message", "external_id": external_id,
            "channel_kind": "email", "channel_slug": "fixture-mailbox", "metadata": metadata,
        })
    }

    async fn original(&self) -> ContentRef {
        self.blobs.put(ORIGINAL.to_vec()).await.unwrap()
    }

    async fn ingest(&self, external_id: &str, original: Option<&ContentRef>) -> Value {
        self.registry
            .dispatch("comm.ingest", self.params(external_id, original))
            .await
            .unwrap()
    }

    fn id(receipt: &Value) -> Uuid {
        Uuid::parse_str(receipt["full_id"].as_str().unwrap()).unwrap()
    }

    async fn note(&self, id: Uuid) -> Note {
        let token = self
            .runtime
            .authorize(Namespace::parse(&self.namespace).unwrap())
            .unwrap();
        self.runtime
            .notes(&token)
            .unwrap()
            .get_note(id)
            .await
            .unwrap()
            .unwrap()
    }

    async fn owner(&self, id: Uuid) -> Option<Attachment> {
        self.runtime
            .core()
            .attachments()
            .unwrap()
            .get_attachment(id, "quarantine-original")
            .await
            .unwrap()
    }

    async fn owner_count(&self) -> i64 {
        let main = self.runtime.core();
        let sql = main.sql();
        let mut reader = sql.reader().await.unwrap();
        match reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM attachments WHERE role = ?1".into(),
                params: vec![SqlValue::Text("quarantine-original".into())],
                label: None,
            })
            .await
            .unwrap()
        {
            Some(SqlValue::Integer(count)) => count,
            other => panic!("expected owner count, got {other:?}"),
        }
    }

    async fn note_count(&self) -> u64 {
        let token = self
            .runtime
            .authorize(Namespace::parse(&self.namespace).unwrap())
            .unwrap();
        self.runtime
            .notes(&token)
            .unwrap()
            .count_notes(&self.namespace, Some("message"))
            .await
            .unwrap()
    }

    async fn execute(&self, sql: &str, params: Vec<SqlValue>) {
        let access = self.runtime.sql();
        let mut writer = access.writer().await.unwrap();
        writer
            .execute(SqlStatement {
                sql: sql.into(),
                params,
                label: None,
            })
            .await
            .unwrap();
    }

    async fn assert_bytes(&self, content_ref: &ContentRef) {
        let bytes = self
            .blobs
            .get_bounded_verified(content_ref, ORIGINAL.len() as u64)
            .await
            .unwrap();
        assert_eq!(bytes.as_slice(), ORIGINAL);
    }

    async fn cleanup(&self, mode: &str) -> Value {
        self.registry.dispatch("comm.cleanup_expired_quarantine", json!({
            "channel_kind": "email",
            "channel_slug": if mode == "legacy_slugless" { "" } else { "fixture-mailbox" }, "mode": mode,
            "as_of_micros": chrono::Utc::now().timestamp_micros() + 120_000_000i64,
        })).await.unwrap()
    }
}

fn attachment(content_ref: &ContentRef) -> NewAttachment {
    NewAttachment {
        role: "quarantine-original".into(),
        content_ref: content_ref.clone(),
        media_type: None,
        size_bytes: Some(ORIGINAL.len() as u64),
    }
}

fn lower_note(fixture: &Fixture, external_id: &str) -> Note {
    Note::new(
        &fixture.namespace,
        "message",
        "controlled lower publication",
    )
    .with_properties(json!({
        "external_id": external_id, "quarantined": true,
        "channel_kind": "email", "channel_slug": "fixture-mailbox", "direction": "inbound",
    }))
}

#[tokio::test]
async fn secondary_ingest_roots_the_actual_note_id_on_main() {
    let fixture = Fixture::new(true);
    let original = fixture.original().await;
    let receipt = fixture.ingest("new-secondary", Some(&original)).await;
    assert_eq!(receipt["quarantine_original_retained"], true);
    let id = Fixture::id(&receipt);
    let note = fixture.note(id).await;
    assert_eq!(
        note.properties.as_ref().unwrap()["quarantine_content_ref"],
        original.to_string()
    );
    let owner = fixture.owner(id).await.unwrap();
    assert_eq!(owner.record_uuid, id);
    assert_eq!(owner.substrate, AttachmentSubstrate::Note);
    assert_eq!(owner.content_ref, original);
    assert!(fixture.runtime.attachments().is_err());
    assert!(fixture
        .runtime
        .backend()
        .attachments()
        .unwrap()
        .get_attachment(id, "quarantine-original")
        .await
        .unwrap()
        .is_none());
    let main = fixture.runtime.core();
    let token = main
        .authorize(Namespace::parse(&fixture.namespace).unwrap())
        .unwrap();
    assert!(main
        .notes(&token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .is_none());
    fixture.assert_bytes(&original).await;
}

#[tokio::test]
async fn secondary_duplicate_replay_keeps_one_note_and_exact_owner() {
    let fixture = Fixture::new(true);
    let original = fixture.original().await;
    let first = fixture.ingest("exact-replay", Some(&original)).await;
    let id = Fixture::id(&first);
    let owner = fixture.owner(id).await.unwrap();
    let replay = fixture.ingest("exact-replay", Some(&original)).await;
    assert_eq!(replay["deduplicated"], true);
    assert_eq!(replay["thread_id"], first["thread_id"]);
    assert_eq!(fixture.note_count().await, 1);
    assert_eq!(fixture.owner_count().await, 1);
    assert_eq!(fixture.owner(id).await.unwrap(), owner);
    fixture.assert_bytes(&original).await;
}

#[tokio::test]
async fn historical_secondary_without_original_is_not_repaired_by_replay() {
    let fixture = Fixture::new(true);
    let first = fixture.ingest("historical-secondary", None).await;
    let id = Fixture::id(&first);
    let before = fixture.note(id).await;
    assert!(before
        .properties
        .as_ref()
        .unwrap()
        .get("quarantine_content_ref")
        .is_none());
    let original = fixture.original().await;
    let replay = fixture
        .ingest("historical-secondary", Some(&original))
        .await;
    assert_eq!(replay["deduplicated"], true);
    assert_eq!(replay["thread_id"], first["thread_id"]);
    let after = fixture.note(id).await;
    assert_eq!(after.properties, before.properties);
    assert_eq!(after.expires_at, before.expires_at);
    assert_eq!(after.updated_at, before.updated_at);
    assert!(fixture.owner(id).await.is_none());
    assert_eq!(fixture.owner_count().await, 0);
    fixture.assert_bytes(&original).await;
}

#[tokio::test]
async fn main_note_only_quarantine_replay_still_repairs_original() {
    let fixture = Fixture::new(false);
    let first = fixture.ingest("historical-main", None).await;
    let id = Fixture::id(&first);
    let original = fixture.original().await;
    let replay = fixture.ingest("historical-main", Some(&original)).await;
    assert_eq!(replay["deduplicated"], true);
    assert_eq!(replay["thread_id"], first["thread_id"]);
    assert_eq!(
        fixture.note(id).await.properties.unwrap()["quarantine_content_ref"],
        original.to_string()
    );
    assert_eq!(fixture.owner(id).await.unwrap().content_ref, original);
}

#[tokio::test]
async fn active_claim_records_a_nonretained_message_instead_of_failing_ingest() {
    let fixture = Fixture::new(true);
    let original = fixture.original().await;
    let main = fixture.runtime.core();
    let sql = main.sql();
    let mut writer = sql.writer().await.unwrap();
    writer
        .execute(SqlStatement {
            sql:
                "INSERT INTO blob_gc_claims(root_key, content_ref, claimed_at) VALUES (?1, ?2, ?3)"
                    .into(),
            params: vec![
                SqlValue::Text("fixture-active-claim".into()),
                SqlValue::Text(original.to_string()),
                SqlValue::Integer(chrono::Utc::now().timestamp_micros()),
            ],
            label: None,
        })
        .await
        .unwrap();
    drop(writer);
    let receipt = fixture.ingest("claimed-original", Some(&original)).await;
    assert_eq!(receipt["quarantine_original_retained"], false);
    assert_eq!(
        receipt["quarantine_original_not_retained_reason"],
        "blob-sweep-active"
    );
    let id = Fixture::id(&receipt);
    let props = fixture.note(id).await.properties.unwrap();
    assert!(props.get("quarantine_content_ref").is_none());
    assert_eq!(
        props["quarantine_original_not_retained_reason"],
        "blob-sweep-active"
    );
    assert!(fixture.owner(id).await.is_none());
    assert_eq!(fixture.note_count().await, 1);
    fixture.assert_bytes(&original).await;
}

#[tokio::test]
async fn duplicate_insert_releases_only_the_new_attempt_owner() {
    let fixture = Fixture::new(true);
    let original = fixture.original().await;
    let receipt = fixture.ingest("duplicate-insert", Some(&original)).await;
    let held_id = Fixture::id(&receipt);
    let held = fixture.owner(held_id).await.unwrap();
    let stored = fixture.note(held_id).await;
    let token = fixture
        .runtime
        .authorize(Namespace::parse(&fixture.namespace).unwrap())
        .unwrap();
    let capability = ChannelIngestCapability::grant_for_direct_composition();
    let outcome = fixture
        .runtime
        .try_create_note_as_trusted_ingest_with_attachment_report(
            &capability,
            &token,
            "message",
            None,
            "losing duplicate attempt",
            stored.properties.clone(),
            attachment(&original),
            Some(RETENTION),
        )
        .await;
    assert!(outcome.result.unwrap().is_none());
    assert_eq!(fixture.owner_count().await, 1);
    assert_eq!(fixture.owner(held_id).await.unwrap(), held);
    assert_eq!(fixture.note_count().await, 1);
    fixture.assert_bytes(&original).await;
}

#[tokio::test]
async fn rolled_back_secondary_insert_releases_its_new_owner() {
    let fixture = Fixture::new(true);
    let original = fixture.original().await;
    fixture.execute("CREATE TRIGGER fixture_reject_secondary_message BEFORE INSERT ON notes \
        WHEN NEW.kind = 'message' BEGIN SELECT RAISE(ABORT, 'fixture secondary insert refused'); END", vec![]).await;
    let error = fixture
        .registry
        .dispatch(
            "comm.ingest",
            fixture.params("rolled-back", Some(&original)),
        )
        .await
        .expect_err("real secondary note INSERT must abort");
    assert!(
        error
            .to_string()
            .contains("fixture secondary insert refused"),
        "{error}"
    );
    assert_eq!(fixture.note_count().await, 0);
    assert_eq!(fixture.owner_count().await, 0);
    fixture.assert_bytes(&original).await;
}

#[tokio::test]
async fn committed_fts_degradation_preserves_secondary_note_and_root() {
    let fixture = Fixture::new(true);
    let original = fixture.original().await;
    let token = fixture
        .runtime
        .authorize(Namespace::parse(&fixture.namespace).unwrap())
        .unwrap();
    drop(fixture.runtime.text_for_notes(&token).unwrap());
    fixture
        .execute(
            "CREATE TRIGGER fixture_reject_fts_map BEFORE INSERT ON fts_notes_rowids \
        BEGIN SELECT RAISE(ABORT, 'fixture FTS upsert refused'); END",
            vec![],
        )
        .await;
    let receipt = fixture
        .ingest("committed-degradation", Some(&original))
        .await;
    assert_eq!(receipt["quarantine_original_retained"], true);
    let failures = receipt["post_commit_degradations"]
        .as_array()
        .expect("actual committed degradation");
    assert!(failures
        .iter()
        .any(|failure| failure["stage"] == "fts_upsert"
            && failure["error"]
                .as_str()
                .is_some_and(|error| error.contains("fixture FTS upsert refused"))));
    let id = Fixture::id(&receipt);
    assert_eq!(
        fixture.note(id).await.properties.unwrap()["quarantine_content_ref"],
        original.to_string()
    );
    assert_eq!(fixture.owner(id).await.unwrap().content_ref, original);
    fixture.assert_bytes(&original).await;
}

#[tokio::test]
async fn mismatched_owner_is_not_overwritten_or_deleted_by_lower_publication() {
    let fixture = Fixture::new(true);
    let original = fixture.original().await;
    let other_bytes = b"different existing owner bytes".to_vec();
    let other = fixture.blobs.put(other_bytes.clone()).await.unwrap();
    let mut note = lower_note(&fixture, "owner-mismatch");
    let held = Attachment::from_new(
        note.id,
        AttachmentSubstrate::Note,
        NewAttachment {
            content_ref: other,
            size_bytes: Some(other_bytes.len() as u64),
            ..attachment(&original)
        },
        note.created_at,
    );
    fixture
        .runtime
        .core()
        .attachments()
        .unwrap()
        .upsert_attachment(held.clone())
        .await
        .unwrap();
    let token = fixture
        .runtime
        .authorize(Namespace::parse(&fixture.namespace).unwrap())
        .unwrap();
    let mut retention = QuarantineOriginalRetention::Retained;
    assert!(fixture
        .runtime
        .test_insert_secondary_quarantine_note(
            &token,
            &mut note,
            attachment(&original),
            &mut retention,
        )
        .await
        .unwrap());
    assert_eq!(retention, QuarantineOriginalRetention::OwnerMismatch);
    assert_eq!(fixture.owner(note.id).await.unwrap(), held);
    let props = fixture.note(note.id).await.properties.unwrap();
    assert!(props.get("quarantine_content_ref").is_none());
    assert_eq!(
        props["quarantine_original_not_retained_reason"],
        "owner-mismatch"
    );
    fixture.assert_bytes(&original).await;
    assert_eq!(
        fixture
            .blobs
            .get_bounded_verified(&held.content_ref, other_bytes.len() as u64)
            .await
            .unwrap(),
        other_bytes
    );
}

#[tokio::test]
async fn preexisting_matching_owner_survives_a_known_duplicate_noncommit() {
    let fixture = Fixture::new(true);
    let original = fixture.original().await;
    let receipt = fixture.ingest("existing-key", Some(&original)).await;
    let stored = fixture.note(Fixture::id(&receipt)).await;
    let mut losing = lower_note(&fixture, "existing-key");
    losing.properties = stored.properties.clone();
    let held = Attachment::from_new(
        losing.id,
        AttachmentSubstrate::Note,
        attachment(&original),
        losing.created_at,
    );
    fixture
        .runtime
        .core()
        .attachments()
        .unwrap()
        .upsert_attachment(held.clone())
        .await
        .unwrap();
    let token = fixture
        .runtime
        .authorize(Namespace::parse(&fixture.namespace).unwrap())
        .unwrap();
    let mut retention = QuarantineOriginalRetention::Retained;
    assert!(!fixture
        .runtime
        .test_insert_secondary_quarantine_note(
            &token,
            &mut losing,
            attachment(&original),
            &mut retention,
        )
        .await
        .unwrap());
    assert_eq!(retention, QuarantineOriginalRetention::Retained);
    assert_eq!(fixture.owner(losing.id).await.unwrap(), held);
    assert_eq!(fixture.note_count().await, 1);
    assert_eq!(fixture.owner_count().await, 2);
}

#[tokio::test]
async fn channel_cleanup_detaches_main_owner_after_secondary_note_deletion() {
    let fixture = Fixture::new(true);
    let original = fixture.original().await;
    let id = Fixture::id(&fixture.ingest("secondary-expiry", Some(&original)).await);
    let cleanup = fixture.cleanup("channel").await;
    assert_eq!(cleanup["deleted"], 1);
    assert_eq!(cleanup["routed_owners_detached"], 1);
    assert_eq!(fixture.note_count().await, 0);
    assert!(fixture.owner(id).await.is_none());
    fixture.assert_bytes(&original).await;
}

#[tokio::test]
async fn main_channel_cleanup_keeps_local_atomic_deletion() {
    let fixture = Fixture::new(false);
    let original = fixture.original().await;
    let id = Fixture::id(&fixture.ingest("main-expiry", Some(&original)).await);
    assert_eq!(fixture.owner(id).await.unwrap().content_ref, original);
    let cleanup = fixture.cleanup("channel").await;
    assert_eq!(cleanup["deleted"], 1);
    assert_eq!(cleanup["routed_owners_detached"], 0);
    assert_eq!(fixture.note_count().await, 0);
    assert!(fixture.owner(id).await.is_none());
    fixture.assert_bytes(&original).await;
}

#[tokio::test]
async fn legacy_slugless_secondary_cleanup_still_detaches_exact_owner() {
    let fixture = Fixture::new(true);
    let original = fixture.original().await;
    let mut legacy = Note::new(&fixture.namespace, "message", "legacy slugless quarantine")
        .with_properties(json!({"quarantined": true, "channel_kind": "email",
            "quarantine_content_ref": original.to_string()}));
    legacy.created_at = chrono::Utc::now().timestamp_micros() - 120_000_000i64;
    legacy.updated_at = legacy.created_at;
    legacy.expires_at = Some(legacy.created_at + 60_000_000i64);
    let token = fixture
        .runtime
        .authorize(Namespace::parse(&fixture.namespace).unwrap())
        .unwrap();
    assert!(fixture
        .runtime
        .notes(&token)
        .unwrap()
        .try_insert_note(legacy.clone())
        .await
        .unwrap());
    fixture
        .runtime
        .core()
        .attachments()
        .unwrap()
        .upsert_attachment(Attachment::from_new(
            legacy.id,
            AttachmentSubstrate::Note,
            attachment(&original),
            legacy.created_at,
        ))
        .await
        .unwrap();
    let cleanup = fixture.cleanup("legacy_slugless").await;
    assert_eq!(cleanup["deleted"], 1);
    assert_eq!(cleanup["routed_owners_detached"], 1);
    assert!(fixture.owner(legacy.id).await.is_none());
    assert_eq!(fixture.note_count().await, 0);
    fixture.assert_bytes(&original).await;
}

#[tokio::test]
async fn current_migrated_gc_refuses_and_preserves_owned_and_unowned_bytes() {
    let fixture = Fixture::new(true);
    let original = fixture.original().await;
    let id = Fixture::id(&fixture.ingest("current-gc", Some(&original)).await);
    let unowned_bytes = b"unowned current-epoch control".to_vec();
    let unowned = fixture.blobs.put(unowned_bytes.clone()).await.unwrap();
    for content_ref in [&original, &unowned] {
        let hex = content_ref.as_str();
        let path = fixture
            .blobs
            .root()
            .join(&hex[..2])
            .join(&hex[2..4])
            .join(hex);
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(3600))
            .unwrap();
    }
    let main = fixture.runtime.core();
    let sql = main.sql();
    for dry_run in [true, false] {
        let error = fixture
            .blobs
            .transactional_orphan_sweep(sql.as_ref(), dry_run)
            .await
            .expect_err("normally migrated current epoch must remain fail-closed");
        assert!(
            matches!(error, StorageError::Unsupported { ref operation, .. }
            if operation == "transactional_orphan_sweep")
        );
        fixture.assert_bytes(&original).await;
        assert_eq!(
            fixture
                .blobs
                .get_bounded_verified(&unowned, unowned_bytes.len() as u64)
                .await
                .unwrap(),
            unowned_bytes
        );
    }
    assert_eq!(fixture.owner(id).await.unwrap().content_ref, original);
}

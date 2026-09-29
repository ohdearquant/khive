use super::*;
use std::sync::Arc;

use async_trait::async_trait;
use khive_channel::DeliveryReceiptBinding;
use khive_db::stores::note::transport::{SenderAssurance, SenderEnvelope};
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use tokio::sync::Notify;

use crate::embedder_registry::EmbedderProvider;

const RECIPIENT_RACE_MODEL: &str = "recipient-race-test";

#[derive(Default)]
struct PausedRecipientEmbedder {
    started: Notify,
    proceed: Notify,
}

#[async_trait]
impl EmbeddingService for PausedRecipientEmbedder {
    async fn embed(
        &self,
        texts: &[String],
        _: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.started.notify_one();
        self.proceed.notified().await;
        Ok(texts.iter().map(|_| vec![0.5; 4]).collect())
    }

    fn supports_model(&self, _: EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        RECIPIENT_RACE_MODEL
    }
}

struct PausedRecipientProvider(Arc<PausedRecipientEmbedder>);

#[async_trait]
impl EmbedderProvider for PausedRecipientProvider {
    fn name(&self) -> &str {
        RECIPIENT_RACE_MODEL
    }

    fn dimensions(&self) -> usize {
        4
    }

    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        Ok(self.0.clone())
    }
}

fn fixture() -> (
    KhiveRuntime,
    NamespaceToken,
    LocalRecipientBinding,
    DeliveryReceiptBinding,
) {
    let runtime = KhiveRuntime::memory().unwrap();
    let local = LocalRecipientBinding {
        actor: "lambda:receiver".into(),
        realm: "example".into(),
        slug: "device".into(),
        agent_id: Uuid::new_v4().to_string(),
        device_id: Uuid::new_v4(),
        key_epoch: 1,
    };
    let binding = DeliveryReceiptBinding {
        protocol_version: 1,
        logical_message_id: Uuid::new_v4(),
        sender_agent_id: Uuid::new_v4().to_string(),
        recipient_agent_id: local.agent_id.clone(),
        recipient_device_id: local.device_id,
        recipient_key_epoch: 1,
        contact_generation: 1,
        delivery_attempt_id: Uuid::new_v4(),
    };
    (runtime, NamespaceToken::local(), local, binding)
}

#[tokio::test]
async fn recipient_embedding_publishes_only_the_live_committed_revision() {
    for change in ["unchanged", "updated", "soft deleted", "hard deleted"] {
        let (runtime, token, local, binding) = fixture();
        let embedder = Arc::new(PausedRecipientEmbedder::default());
        runtime.register_embedder(PausedRecipientProvider(embedder.clone()));
        let vectors = runtime
            .vectors_for_model(&token, RECIPIENT_RACE_MODEL)
            .unwrap();

        let ingest_runtime = runtime.clone();
        let ingest_token = token.clone();
        let ingest = tokio::spawn(async move {
            ingest_runtime
                .ingest_verified_recipient(
                    &ingest_token,
                    &local,
                    InboundReceiptTicket::new(binding, 1),
                    payload(None),
                    vec![],
                )
                .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            embedder.started.notified(),
        )
        .await
        .expect("recipient commit must reach embedding");
        // The receipt is durable before embedding starts. Change that exact note
        // while its original vector is still being computed.
        let notes = runtime
            .list_notes(&token, Some("message"), 10, 0)
            .await
            .unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].content, "hello");
        let note_id = notes[0].id;
        match change {
            "updated" => {
                let patch = crate::curation::NotePatch::new(
                    None,
                    Some("newer content".into()),
                    None,
                    None,
                    None,
                )
                .with_write_options(crate::note_write::NoteWriteOptions {
                    expected_version: Some(1),
                    embed: Some(false),
                    ..Default::default()
                });
                assert_eq!(
                    runtime
                        .update_note(&token, note_id, patch)
                        .await
                        .unwrap()
                        .version,
                    2
                );
            }
            "soft deleted" | "hard deleted" => {
                assert!(runtime
                    .delete_note(&token, note_id, change == "hard deleted")
                    .await
                    .unwrap());
            }
            "unchanged" => {}
            _ => unreachable!(),
        }
        embedder.proceed.notify_one();
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), ingest)
            .await
            .expect("recipient ingest must finish")
            .unwrap()
            .unwrap();
        assert_eq!(result.note_id, note_id);
        assert_eq!(
            vectors.count().await.unwrap(),
            u64::from(change == "unchanged"),
            "{change}"
        );
    }
}
fn payload(correlation: Option<String>) -> VerifiedInboundContent {
    VerifiedInboundContent::Message {
        content: "hello".into(),
        subject: None,
        kind: None,
        in_reply_to: None,
        correlation,
        sent_at: "2026-09-01T12:34:56Z".into(),
    }
}

fn reply_payload(parent: Uuid) -> VerifiedInboundContent {
    let mut message = payload(None);
    let VerifiedInboundContent::Message {
        kind, in_reply_to, ..
    } = &mut message
    else {
        unreachable!("payload fixture is a message")
    };
    *kind = Some(DeclaredMessageKind::Ask);
    *in_reply_to = Some(parent);
    message
}

async fn outbound_parent(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    local: &LocalRecipientBinding,
    remote_agent_id: &str,
    transport_recipient_agent_id: &str,
) -> (Uuid, Uuid) {
    let mut note = Note::new(token.namespace().as_str(), "message", "outbound parent");
    note.properties = Some(json!({
        "direction": "outbound",
        "from_actor": local.actor,
        "to_actor": format!("khive1:example/{remote_agent_id}"),
    }));
    let note_id = note.id;
    runtime
        .notes(token)
        .unwrap()
        .upsert_note(note)
        .await
        .unwrap();

    let logical_message_id = Uuid::new_v4();
    runtime
        .create_sender_transport(
            token,
            SenderEnvelope {
                namespace: "ignored-caller-attribution".into(),
                logical_message_id,
                outbound_note_id: note_id,
                kind: "khive".into(),
                slug: "device".into(),
                credential_ref: "keys/device".into(),
                recipient_address: format!("khive1:example/{transport_recipient_agent_id}"),
                protocol_version: 1,
                sender_agent_id: local.agent_id.clone(),
                sender_assurance: SenderAssurance::Claimed,
                recipient_agent_id: transport_recipient_agent_id.into(),
                recipient_device_id: Uuid::new_v4(),
                recipient_key_epoch: 1,
                contact_generation: 1,
                sender_key_epoch: 1,
                recipient_key_fingerprint: "ab".repeat(32),
                enc: vec![1; 32],
                ciphertext: vec![2],
            },
        )
        .await
        .unwrap();
    (note_id, logical_message_id)
}
#[tokio::test]
async fn inbound_message_preserves_sender_sent_at_and_local_received_at() {
    let (runtime, token, local, binding) = fixture();
    let before = chrono::Utc::now();
    let result = runtime
        .ingest_verified_recipient(
            &token,
            &local,
            InboundReceiptTicket::new(binding, 1),
            payload(None),
            vec![],
        )
        .await
        .unwrap();
    let after = chrono::Utc::now();
    let props = result.note.unwrap().properties.unwrap();
    assert_eq!(props["sent_at"], "2026-09-01T12:34:56Z");
    let received_at = chrono::DateTime::parse_from_rfc3339(props["received_at"].as_str().unwrap())
        .unwrap()
        .with_timezone(&chrono::Utc);
    assert!(received_at >= before && received_at <= after);
}

#[tokio::test]
async fn delivered_replay_is_acked_before_gate_or_timestamp_revalidation() {
    let (runtime, token, local, binding) = fixture();
    let first = runtime
        .ingest_verified_recipient(
            &token,
            &local,
            InboundReceiptTicket::new(binding.clone(), 1),
            payload(None),
            vec![],
        )
        .await
        .unwrap();
    assert!(first.created);

    let mut retry = binding.clone();
    retry.delivery_attempt_id = Uuid::new_v4();
    let fake_secret = format!("ghp_{}", "A".repeat(36));
    assert!(matches!(
        crate::secret_gate::check_at(&fake_secret, "note", "content"),
        Err(RuntimeError::SecretDetected(_))
    ));
    let replay = runtime
        .ingest_verified_recipient(
            &token,
            &local,
            InboundReceiptTicket::new(retry, 1),
            VerifiedInboundContent::Message {
                content: fake_secret,
                subject: None,
                kind: None,
                in_reply_to: None,
                correlation: None,
                sent_at: "2026-09-01T12:34:56Z".into(),
            },
            vec![],
        )
        .await
        .unwrap();
    assert!(!replay.created);
    assert_eq!(replay.note_id, first.note_id);
    assert_eq!(replay.disposition, RecipientDisposition::Stored);
    assert!(replay.note.is_none());

    let mut invalid_timestamp_retry = binding;
    invalid_timestamp_retry.delivery_attempt_id = Uuid::new_v4();
    let replay = runtime
        .ingest_verified_recipient(
            &token,
            &local,
            InboundReceiptTicket::new(invalid_timestamp_retry, 1),
            VerifiedInboundContent::Message {
                content: "redelivered body".into(),
                subject: None,
                kind: None,
                in_reply_to: None,
                correlation: None,
                sent_at: "not-a-timestamp".into(),
            },
            b"not JSON".to_vec(),
        )
        .await
        .unwrap();
    assert!(!replay.created);
    assert_eq!(replay.note_id, first.note_id);
    assert_eq!(replay.disposition, RecipientDisposition::Stored);
    let stored = runtime
        .notes(&token)
        .unwrap()
        .get_note(first.note_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.content, "hello");
    let backend = runtime.backend();
    let writer = backend.pool().writer().unwrap();
    let ack_count: i64 = writer
        .conn()
        .query_row("SELECT count(*) FROM comm_ack_work", [], |row| row.get(0))
        .unwrap();
    assert_eq!(ack_count, 3, "every new attempt receives its own ack");
    let replay_count: i64 = writer
        .conn()
        .query_row("SELECT count(*) FROM comm_recipient_replay", [], |row| {
            row.get(0)
        })
        .unwrap();
    let quarantine_count: i64 = writer
        .conn()
        .query_row(
            "SELECT count(*) FROM comm_recipient_quarantine",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!((replay_count, quarantine_count), (1, 0));
}

#[tokio::test]
async fn inbound_message_persists_declared_or_unspecified_kind() {
    for (declared, expected) in [
        (Some(DeclaredMessageKind::Announce), "announce"),
        (Some(DeclaredMessageKind::Report), "report"),
        (Some(DeclaredMessageKind::Ask), "ask"),
        (None, "unspecified"),
    ] {
        let (runtime, token, local, binding) = fixture();
        let mut message = payload(None);
        let VerifiedInboundContent::Message { kind, .. } = &mut message else {
            unreachable!("payload fixture is a message")
        };
        *kind = declared;
        let result = runtime
            .ingest_verified_recipient(
                &token,
                &local,
                InboundReceiptTicket::new(binding, 1),
                message,
                vec![],
            )
            .await
            .unwrap();
        let persisted = runtime
            .notes(&token)
            .unwrap()
            .get_note(result.note_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(persisted.properties.unwrap()["message_kind"], expected);
    }
}
#[tokio::test]
async fn invalid_sender_timestamp_is_quarantined_without_message_body() {
    for sent_at in ["not-a-timestamp", "2026-09-01T12:34:56+01:00"] {
        let (runtime, token, local, binding) = fixture();
        let result = runtime
            .ingest_verified_recipient(
                &token,
                &local,
                InboundReceiptTicket::new(binding, 1),
                VerifiedInboundContent::Message {
                    content: "private body".into(),
                    subject: None,
                    kind: Some(DeclaredMessageKind::Ask),
                    in_reply_to: None,
                    correlation: None,
                    sent_at: sent_at.into(),
                },
                b"{\"delivery\":\"opaque\"}".to_vec(),
            )
            .await
            .unwrap();
        assert_eq!(result.disposition, RecipientDisposition::Quarantined);
        let note = result.note.unwrap();
        assert!(!note.content.contains("private body"));
        let props = note.properties.unwrap();
        assert_eq!(props["quarantined"], true);
        assert!(props.get("sent_at").is_none());
        assert!(props.get("message_kind").is_none());
        assert!(props["received_at"].is_string());
    }
}
#[tokio::test]
async fn local_binding_fixes_recipient_and_derives_sender() {
    let (runtime, token, local, binding) = fixture();
    let mut wrong = binding.clone();
    wrong.recipient_agent_id = Uuid::new_v4().to_string();
    assert!(
        runtime
            .ingest_verified_recipient(
                &token,
                &local,
                InboundReceiptTicket::new(wrong, 1),
                payload(None),
                vec![]
            )
            .await
            .is_err(),
        "remote recipient must not redirect local binding"
    );
    let result = runtime
        .ingest_verified_recipient(
            &token,
            &local,
            InboundReceiptTicket::new(binding.clone(), 1),
            payload(None),
            vec![],
        )
        .await
        .unwrap();
    let note = result.note.unwrap();
    let props = note.properties.unwrap();
    assert_eq!(
        props["to_actor"], local.actor,
        "recipient actor must come from local binding"
    );
    assert_eq!(
        props["from_actor"],
        format!("khive1:example/{}", binding.sender_agent_id)
    );
}
#[tokio::test]
async fn correlation_is_confined_to_authenticated_pair() {
    for same_pair in [false, true] {
        let (runtime, token, local, binding) = fixture();
        let thread = Uuid::new_v4();
        let mut root = Note::new("local", "message", "root");
        root.properties = Some(
            json!({"external_id":"correlation","thread_id":thread,"from_actor":local.actor,"to_actor":if same_pair {format!("khive1:example/{}",binding.sender_agent_id)} else {"lambda:other".into()},"direction":"outbound"}),
        );
        runtime
            .notes(&token)
            .unwrap()
            .upsert_note(root)
            .await
            .unwrap();
        let result = runtime
            .ingest_verified_recipient(
                &token,
                &local,
                InboundReceiptTicket::new(binding, 1),
                payload(Some("correlation".into())),
                vec![],
            )
            .await
            .unwrap();
        let note = result.note.unwrap();
        assert_eq!(
            note.properties.as_ref().unwrap()["message_kind"],
            "unspecified",
            "thread correlation alone does not prove a reply"
        );
        assert_eq!(
            note.properties.unwrap()["thread_id"],
            if same_pair {
                thread.to_string()
            } else {
                note.id.to_string()
            },
            "thread must be confined to authenticated pair"
        );
    }
}

#[tokio::test]
async fn live_outbound_transport_parent_derives_reply_over_declared_ask() {
    let (runtime, token, local, binding) = fixture();
    let (parent_note_id, parent) = outbound_parent(
        &runtime,
        &token,
        &local,
        &binding.sender_agent_id,
        &binding.sender_agent_id,
    )
    .await;
    let result = runtime
        .ingest_verified_recipient(
            &token,
            &local,
            InboundReceiptTicket::new(binding.clone(), 1),
            reply_payload(parent),
            vec![],
        )
        .await
        .unwrap();
    assert_eq!(
        result.note.as_ref().unwrap().properties.as_ref().unwrap()["message_kind"],
        "reply"
    );
    assert_eq!(
        result.note.as_ref().unwrap().properties.as_ref().unwrap()["in_reply_to"],
        parent.to_string()
    );
    let note_id = result.note_id;
    assert!(runtime
        .delete_note(&token, parent_note_id, true)
        .await
        .unwrap());
    let mut retry = binding;
    retry.delivery_attempt_id = Uuid::new_v4();
    let replay = runtime
        .ingest_verified_recipient(
            &token,
            &local,
            InboundReceiptTicket::new(retry, 1),
            payload(None),
            vec![],
        )
        .await
        .unwrap();
    assert!(!replay.created, "retry must keep the first committed kind");
    let stored = runtime
        .notes(&token)
        .unwrap()
        .get_note(note_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.properties.unwrap()["message_kind"], "reply");
}

#[tokio::test]
async fn outbound_parent_for_another_recipient_does_not_derive_reply() {
    let (runtime, token, local, binding) = fixture();
    let (_, parent) = outbound_parent(
        &runtime,
        &token,
        &local,
        &binding.sender_agent_id,
        &Uuid::new_v4().to_string(),
    )
    .await;
    let result = runtime
        .ingest_verified_recipient(
            &token,
            &local,
            InboundReceiptTicket::new(binding, 1),
            reply_payload(parent),
            vec![],
        )
        .await
        .unwrap();
    let props = result.note.unwrap().properties.unwrap();
    assert_eq!(props["message_kind"], "ask");
    assert_eq!(props["in_reply_to"], parent.to_string());
}

#[tokio::test]
async fn deleted_outbound_parent_falls_back_to_declared_kind() {
    for hard in [false, true] {
        let (runtime, token, local, binding) = fixture();
        let (note_id, parent) = outbound_parent(
            &runtime,
            &token,
            &local,
            &binding.sender_agent_id,
            &binding.sender_agent_id,
        )
        .await;
        assert!(runtime.delete_note(&token, note_id, hard).await.unwrap());
        let result = runtime
            .ingest_verified_recipient(
                &token,
                &local,
                InboundReceiptTicket::new(binding, 1),
                reply_payload(parent),
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(
            result.note.unwrap().properties.unwrap()["message_kind"],
            "ask",
            "deleted outbound note cannot authorize reply, hard={hard}"
        );
    }
}

#[tokio::test]
async fn quarantine_notification_is_body_free_and_replay_is_verbatim() {
    let (runtime, token, local, binding) = fixture();
    let bytes = b"{ \"ciphertext\": \"opaque-sensitive-payload\" }".to_vec();
    let result = runtime
        .ingest_verified_recipient(
            &token,
            &local,
            InboundReceiptTicket::new(binding, 1),
            VerifiedInboundContent::Quarantine {
                reason: QuarantineReason::InvalidPlaintext,
            },
            bytes.clone(),
        )
        .await
        .unwrap();
    assert_eq!(result.disposition, RecipientDisposition::Quarantined);
    let note = result.note.unwrap();
    assert!(!note.content.contains("opaque-sensitive-payload"));
    assert!(!note
        .properties
        .unwrap()
        .to_string()
        .contains("opaque-sensitive-payload"));
    let stored: Vec<u8> = runtime
        .backend()
        .pool()
        .writer()
        .unwrap()
        .conn()
        .query_row(
            "SELECT delivery_item FROM comm_recipient_quarantine",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, bytes);
}

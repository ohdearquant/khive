use super::*;
use khive_channel::DeliveryReceiptBinding;

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

async fn commit_stored(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    local: &LocalRecipientBinding,
    binding: &DeliveryReceiptBinding,
) -> RecipientCommitResult {
    runtime
        .ingest_verified_recipient(
            token,
            local,
            InboundReceiptTicket::new(binding.clone(), 1),
            VerifiedInboundContent::Message {
                content: "acknowledgement journal message".into(),
                subject: None,
                kind: None,
                in_reply_to: None,
                correlation: None,
                sent_at: "2026-09-01T12:34:56Z".into(),
            },
            vec![],
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn due_acknowledgement_entry_preserves_committed_identity_and_finishes_once() {
    let (runtime, token, local, binding) = fixture();
    let committed = commit_stored(&runtime, &token, &local, &binding).await;
    assert!(committed.created);
    let due = runtime.due_acknowledgements(i64::MAX, 10).await.unwrap();
    assert_eq!(due.len(), 1);
    let entry = &due[0];
    assert_eq!(entry.binding(), &serde_json::to_value(&binding).unwrap());
    assert_eq!(entry.disposition(), RecipientDisposition::Stored);
    assert_eq!(entry.delivery_attempt_id(), binding.delivery_attempt_id);
    assert_eq!(entry.attempt_count(), 0);
    assert_eq!(entry.not_before(), None);
    assert!(runtime
        .finish_acknowledgement(entry.delivery_attempt_id())
        .await
        .unwrap());
    assert!(!runtime
        .finish_acknowledgement(entry.delivery_attempt_id())
        .await
        .unwrap());
    assert!(runtime
        .due_acknowledgements(i64::MAX, 10)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn due_acknowledgement_retry_metadata_survives_the_runtime_read() {
    let (runtime, token, local, binding) = fixture();
    commit_stored(&runtime, &token, &local, &binding).await;
    let now = chrono::Utc::now().timestamp_micros();
    let not_before = now + 1_000_000;
    assert!(runtime
        .record_acknowledgement_failed_try(binding.delivery_attempt_id, not_before)
        .await
        .unwrap());
    assert!(runtime
        .due_acknowledgements(now, 10)
        .await
        .unwrap()
        .is_empty());
    let due = runtime.due_acknowledgements(not_before, 10).await.unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].attempt_count(), 1);
    assert_eq!(due[0].not_before(), Some(not_before));
    assert_eq!(due[0].delivery_attempt_id(), binding.delivery_attempt_id);
    assert_eq!(due[0].binding(), &serde_json::to_value(&binding).unwrap());
}

#[tokio::test]
async fn retired_acknowledgement_is_terminal_and_keeps_the_committed_message() {
    let (runtime, token, local, binding) = fixture();
    let committed = commit_stored(&runtime, &token, &local, &binding).await;
    let note_id = committed.note_id.unwrap();
    let before = runtime
        .list_notes(&token, Some("message"), 10, 0)
        .await
        .unwrap();
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].id, note_id);
    assert!(runtime
        .retire_acknowledgement(
            binding.delivery_attempt_id,
            AcknowledgementRetirementReason::PermanentTransport,
        )
        .await
        .unwrap());
    assert!(!runtime
        .finish_acknowledgement(binding.delivery_attempt_id)
        .await
        .unwrap());
    assert!(!runtime
        .record_acknowledgement_failed_try(binding.delivery_attempt_id, i64::MAX)
        .await
        .unwrap());
    assert!(!runtime
        .retire_acknowledgement(
            binding.delivery_attempt_id,
            AcknowledgementRetirementReason::PermanentTransport,
        )
        .await
        .unwrap());
    assert!(runtime
        .due_acknowledgements(i64::MAX, 10)
        .await
        .unwrap()
        .is_empty());
    let replay = commit_stored(&runtime, &token, &local, &binding).await;
    assert!(!replay.created);
    assert_eq!(replay.note_id, Some(note_id));
    let after = runtime
        .list_notes(&token, Some("message"), 10, 0)
        .await
        .unwrap();
    assert_eq!(after.len(), before.len());
    assert_eq!(after[0].id, before[0].id);
    assert_eq!(after[0].content, before[0].content);
    assert_eq!(after[0].version, before[0].version);
    assert_eq!(after[0].created_at, before[0].created_at);
    assert_eq!(after[0].updated_at, before[0].updated_at);
    assert!(runtime
        .due_acknowledgements(i64::MAX, 10)
        .await
        .unwrap()
        .is_empty());
}

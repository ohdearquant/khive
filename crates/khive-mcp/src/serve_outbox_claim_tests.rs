use super::*;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use khive_channel::{Channel, ChannelEnvelope, ChannelError};
use khive_runtime::{KhiveRuntime, Namespace, NamespaceToken, NotePatch, RuntimeError};
use khive_storage::{Note, StorageCapability, StorageError, WriterTaskRequestState};
use std::sync::Mutex;

#[derive(Default)]
struct RecordingChannel {
    sent: Mutex<Vec<ChannelEnvelope>>,
}

#[async_trait]
impl Channel for RecordingChannel {
    fn kind(&self) -> &'static str {
        "recording"
    }
    async fn send(&self, envelope: ChannelEnvelope) -> Result<(), ChannelError> {
        self.sent.lock().unwrap().push(envelope);
        Ok(())
    }
    async fn poll(&self, _since: DateTime<Utc>) -> Result<Vec<ChannelEnvelope>, ChannelError> {
        Ok(Vec::new())
    }
}

async fn fixture() -> (KhiveRuntime, NamespaceToken, uuid::Uuid) {
    let runtime = KhiveRuntime::memory().unwrap();
    runtime.install_pack_owned_note_kinds(vec!["message".to_string()]);
    let token = runtime.authorize(Namespace::local()).unwrap();
    let mut note = Note::new("local", "message", "outbox claim control");
    note.properties = Some(
        serde_json::json!({"direction":"outbound", "to_actor":"email:recipient@example.com", "subject":"subject"}),
    );
    let id = note.id;
    runtime
        .notes(&token)
        .unwrap()
        .upsert_note(note)
        .await
        .unwrap();
    (runtime, token, id)
}

async fn cycle(runtime: &KhiveRuntime, channel: &RecordingChannel) {
    assert!(
        !channel_outbox_once(
            channel,
            runtime,
            &Namespace::local(),
            "sender@example.com",
            "example.com",
            &["recipient@example.com".to_string()]
        )
        .await
    );
}

fn invalid_storage_input() -> StorageError {
    StorageError::InvalidInput {
        capability: StorageCapability::Notes,
        operation: "claim_external_id".into(),
        message: "owner stamp refused".to_string(),
    }
}

#[test]
fn claim_failure_classification_uses_types_not_error_text() {
    for error in [
        RuntimeError::InvalidInput("refused".into()),
        RuntimeError::from(khive_types::KhiveError::invalid_input("refused")),
        RuntimeError::Storage(invalid_storage_input()),
        RuntimeError::Storage(StorageError::WriterTaskRequestFailed {
            request_state: WriterTaskRequestState::TransactionRolledBack,
            source: Box::new(invalid_storage_input()),
        }),
    ] {
        assert!(outbound_claim_failure_is_permanent(&error), "{error}");
    }
    for error in [
        RuntimeError::Storage(StorageError::WriteQueueFull { timeout_ms: 10 }),
        RuntimeError::Storage(StorageError::WriterTaskBusy { timeout_ms: 10 }),
        RuntimeError::Storage(StorageError::WriterTaskRequestFailed {
            request_state: WriterTaskRequestState::TransactionRolledBack,
            source: Box::new(StorageError::WriterTaskBusy { timeout_ms: 10 }),
        }),
        RuntimeError::from(khive_types::KhiveError::conflict("claim raced")),
        RuntimeError::Internal("invalid input: text does not establish permanence".into()),
    ] {
        assert!(!outbound_claim_failure_is_permanent(&error), "{error}");
    }
}

// Uses the exact error-recording helper called by the loop. No transport is
// contacted; real subsequent outbox scans must exclude the persisted failure.
#[tokio::test]
async fn permanent_claim_failure_is_visible_and_not_retried() {
    for error in [
        RuntimeError::InvalidInput("owner stamp refused".into()),
        RuntimeError::Storage(invalid_storage_input()),
    ] {
        let (runtime, token, id) = fixture().await;
        record_outbound_claim_failure(&runtime, &token, id, &error)
            .await
            .unwrap();
        let before = runtime
            .notes(&token)
            .unwrap()
            .get_note(id)
            .await
            .unwrap()
            .unwrap();
        let props = before.properties.as_ref().unwrap();
        assert_eq!(props["delivery"], "failed");
        assert!(props["failed_at"].as_str().is_some());
        assert_eq!(props["last_error"], error.to_string());
        assert!(props.get("external_id").is_none());
        assert!(props.get("next_attempt_at").is_none());
        let channel = RecordingChannel::default();
        cycle(&runtime, &channel).await;
        cycle(&runtime, &channel).await;
        assert!(channel.sent.lock().unwrap().is_empty());
        let after = runtime
            .notes(&token)
            .unwrap()
            .get_note(id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(after).unwrap(),
            serde_json::to_value(before).unwrap()
        );
    }
}

#[tokio::test]
async fn transient_claim_failure_backs_off_then_claims_and_delivers() {
    let (runtime, token, id) = fixture().await;
    let pressure = RuntimeError::Storage(StorageError::WriteQueueFull { timeout_ms: 10 });
    record_outbound_claim_failure(&runtime, &token, id, &pressure)
        .await
        .unwrap();
    let deferred = runtime
        .notes(&token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .unwrap();
    let properties = deferred.properties.as_ref().unwrap();
    assert_eq!(properties["delivery_attempts"], 1);
    assert!(
        DateTime::parse_from_rfc3339(properties["next_attempt_at"].as_str().unwrap()).unwrap()
            > Utc::now()
    );
    assert!(properties.get("delivery").is_none());
    let channel = RecordingChannel::default();
    cycle(&runtime, &channel).await;
    assert!(channel.sent.lock().unwrap().is_empty());

    // Make the existing persisted deadline due without sleeping or bypassing
    // the public metadata-update path.
    runtime
        .update_note(&token, id, {
            let mut patch = NotePatch::default();
            patch.properties = Some(serde_json::json!({"next_attempt_at":"2000-01-01T00:00:00Z"}));
            patch
        })
        .await
        .unwrap();
    cycle(&runtime, &channel).await;
    let delivered = runtime
        .notes(&token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .unwrap();
    let properties = delivered.properties.as_ref().unwrap();
    let expected = format!("<{id}@example.com>");
    assert_eq!(properties["external_id"], expected);
    assert_eq!(properties["delivery"], "delivered");
    assert!(properties.get("next_attempt_at").is_none());
    assert!(properties.get("delivery_attempts").is_none());
    {
        let sent = channel.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].message_id.as_deref(), Some(expected.as_str()));
    }
    let error = runtime
        .update_note(&token, id, {
            let mut patch = NotePatch::default();
            patch.properties =
                Some(serde_json::json!({"external_id":"<caller-forged@example.com>"}));
            patch
        })
        .await
        .unwrap_err();
    assert!(matches!(error, RuntimeError::InvalidInput(_)));
    let after_refusal = runtime
        .notes(&token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(after_refusal).unwrap(),
        serde_json::to_value(delivered).unwrap()
    );
}

#[tokio::test]
async fn already_claimed_refusal_preserves_winner_and_remains_sendable() {
    let (runtime, token, id) = fixture().await;
    let claimed = runtime
        .claim_outbound_message_external_id(&token, id, "<winner@example.com>".into())
        .await
        .unwrap();
    let error = runtime
        .claim_outbound_message_external_id(&token, id, "<loser@example.com>".into())
        .await
        .unwrap_err();
    assert!(outbound_claim_failure_is_permanent(&error));
    record_outbound_claim_failure(&runtime, &token, id, &error)
        .await
        .unwrap();
    // A transient conflict from a racing claim also leaves the winner intact.
    record_outbound_claim_failure(
        &runtime,
        &token,
        id,
        &RuntimeError::from(khive_types::KhiveError::conflict("claim raced")),
    )
    .await
    .unwrap();
    let untouched = runtime
        .notes(&token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(untouched).unwrap(),
        serde_json::to_value(claimed).unwrap()
    );
    let channel = RecordingChannel::default();
    cycle(&runtime, &channel).await;
    assert_eq!(
        channel.sent.lock().unwrap()[0].message_id.as_deref(),
        Some("<winner@example.com>")
    );
    let delivered = runtime
        .notes(&token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        delivered.properties.as_ref().unwrap()["delivery"],
        "delivered"
    );
    assert_eq!(
        delivered.properties.as_ref().unwrap()["external_id"],
        "<winner@example.com>"
    );
}

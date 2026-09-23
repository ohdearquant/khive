use super::*;
use crate::StorageBackend;
use khive_storage::{DeleteMode, Note, NoteStore};

fn fixture() -> (StorageBackend, SenderEnvelope) {
    let backend = StorageBackend::memory().unwrap();
    crate::run_migrations(backend.pool().writer().unwrap().conn_mut()).unwrap();
    let envelope = SenderEnvelope {
        namespace: "local".into(),
        logical_message_id: Uuid::new_v4(),
        outbound_note_id: Uuid::new_v4(),
        kind: "khive".into(),
        slug: "local-device".into(),
        credential_ref: "keys/local-device".into(),
        recipient_address: format!("khive1:example/{}", Uuid::nil()),
        protocol_version: 1,
        sender_agent_id: Uuid::new_v4().to_string(),
        recipient_agent_id: Uuid::nil().to_string(),
        recipient_device_id: Uuid::new_v4(),
        recipient_key_epoch: 1,
        contact_generation: 1,
        sender_key_epoch: 1,
        recipient_key_fingerprint: "ab".repeat(32),
        enc: vec![1; 32],
        ciphertext: vec![2, 0, 255],
    };
    (backend, envelope)
}

#[tokio::test]
async fn idempotent_create_and_retry_preserve_bytes() {
    let (backend, envelope) = fixture();
    let store = SenderTransportStore::new(backend.pool_arc());
    let first = store.create(envelope.clone(), false).await.unwrap();
    assert_eq!(
        first,
        store
            .create(envelope.clone(), false)
            .await
            .expect("exact create must be idempotent"),
        "exact create must preserve record"
    );
    assert_eq!(
        store
            .list_pending("local", "khive", "local-device", i64::MAX, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    let changed = SenderEnvelope {
        ciphertext: vec![9],
        ..envelope.clone()
    };
    assert!(store.create(changed, false).await.is_err());
    store
        .record_failure(envelope.key(), FailureClass::Transient, Some(42))
        .await
        .unwrap();
    let retry = store.get(envelope.key()).await.unwrap().unwrap();
    assert_eq!(retry.envelope, envelope);
    assert_eq!(retry.attempt_count, 1);
    assert_eq!(retry.next_retry_at, Some(42));
    assert_eq!(retry.state, TransportState::Pending);
}

#[tokio::test]
async fn exact_slug_and_auth_hold_pending() {
    let (backend, envelope) = fixture();
    let store = SenderTransportStore::new(backend.pool_arc());
    store.create(envelope.clone(), false).await.unwrap();
    let other = SenderEnvelope {
        logical_message_id: Uuid::new_v4(),
        slug: "other".into(),
        ..envelope.clone()
    };
    store.create(other, false).await.unwrap();
    store
        .record_failure(envelope.key(), FailureClass::Authentication, None)
        .await
        .unwrap();
    let row = store.get(envelope.key()).await.unwrap().unwrap();
    assert_eq!(
        row.state,
        TransportState::Pending,
        "authentication must remain pending"
    );
    assert_eq!(
        row.attempt_count, 0,
        "authentication must not count as a failed attempt"
    );
    let rows = store
        .list_pending("local", "khive", "local-device", i64::MAX, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "pending scan must isolate exact slug");
    assert_eq!(rows[0].envelope.key(), envelope.key());
    store
        .hold(envelope.key(), Some(HoldReason::InsufficientCredit))
        .await
        .unwrap();
    assert!(store
        .list_pending("local", "khive", "local-device", i64::MAX, 10)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        store.get(envelope.key()).await.unwrap().unwrap().state,
        TransportState::Pending
    );
    store.hold(envelope.key(), None).await.unwrap();
    assert_eq!(
        store
            .list_pending("local", "khive", "local-device", i64::MAX, 10)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn records_survive_note_deletion_and_confirmed_epoch_change() {
    let (backend, envelope) = fixture();
    let store = SenderTransportStore::new(backend.pool_arc());
    let notes = SqlNoteStore::new(backend.pool_arc(), false);
    let mut note = Note::new("local", "message", "outbound");
    note.id = envelope.outbound_note_id;
    notes.upsert_note(note).await.unwrap();
    store.create(envelope.clone(), false).await.unwrap();
    notes
        .delete_note(envelope.outbound_note_id, DeleteMode::Hard)
        .await
        .unwrap();
    assert!(store.get(envelope.key()).await.unwrap().is_some());
    let next = SenderEnvelope {
        recipient_key_epoch: 2,
        enc: vec![3; 32],
        ..envelope.clone()
    };
    assert!(
        store.create(next.clone(), false).await.is_err(),
        "new envelope requires explicit confirmation"
    );
    assert!(store.create(next.clone(), true).await.is_err());
    store
        .hold(envelope.key(), Some(HoldReason::RecipientKeyChanged))
        .await
        .unwrap();
    assert!(
        store.create(next.clone(), false).await.is_err(),
        "held envelope still requires explicit confirmation"
    );
    store.create(next.clone(), true).await.unwrap();
    assert!(store.get(envelope.key()).await.unwrap().is_some());
    let pending = store
        .list_pending("local", "khive", "local-device", i64::MAX, 10)
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].envelope, next);
}

#[tokio::test]
async fn verified_outcome_overrides_failed_but_not_conflicting_receipt() {
    let (backend, envelope) = fixture();
    let store = SenderTransportStore::new(backend.pool_arc());
    store.create(envelope.clone(), false).await.unwrap();
    store
        .record_failure(envelope.key(), FailureClass::Permanent, None)
        .await
        .unwrap();
    let receipt = receipt(&envelope);
    store
        .accept_receipt(
            envelope.key(),
            TransportState::RecipientStored,
            receipt.clone(),
        )
        .await
        .expect("verified receipt must override local failure");
    let row = store.get(envelope.key()).await.unwrap().unwrap();
    assert_eq!(row.receipt, Some(receipt.clone()));
    assert_eq!(row.state, TransportState::RecipientStored);
    assert!(store
        .accept_receipt(
            envelope.key(),
            TransportState::RecipientQuarantined,
            receipt
        )
        .await
        .is_err());
}

fn receipt(e: &SenderEnvelope) -> Value {
    serde_json::json!({"binding":{"protocol_version":e.protocol_version,"logical_message_id":e.logical_message_id,"sender_agent_id":e.sender_agent_id,"recipient_agent_id":e.recipient_agent_id,"recipient_device_id":e.recipient_device_id,"recipient_key_epoch":e.recipient_key_epoch,"contact_generation":e.contact_generation,"delivery_attempt_id":Uuid::new_v4()},"disposition":"stored","signature":[1]})
}

#[tokio::test]
async fn every_known_binding_field_and_disposition_must_match() {
    let (backend, e) = fixture();
    let store = SenderTransportStore::new(backend.pool_arc());
    store.create(e.clone(), false).await.unwrap();
    for field in [
        "protocol_version",
        "logical_message_id",
        "sender_agent_id",
        "recipient_agent_id",
        "recipient_device_id",
        "recipient_key_epoch",
        "contact_generation",
    ] {
        let mut bad = receipt(&e);
        bad["binding"][field] = Value::Null;
        assert!(
            store
                .accept_receipt(e.key(), TransportState::RecipientStored, bad)
                .await
                .is_err(),
            "accepted mismatched {field}"
        );
        assert_eq!(
            store.get(e.key()).await.unwrap().unwrap().state,
            TransportState::Pending
        );
    }
    assert!(
        store
            .accept_receipt(e.key(), TransportState::RecipientQuarantined, receipt(&e))
            .await
            .is_err(),
        "receipt disposition must match target"
    );
}

#[tokio::test]
async fn late_receipt_stops_new_epoch_retry() {
    let (backend, e) = fixture();
    let store = SenderTransportStore::new(backend.pool_arc());
    store.create(e.clone(), false).await.unwrap();
    store
        .hold(e.key(), Some(HoldReason::RecipientKeyChanged))
        .await
        .unwrap();
    let next = SenderEnvelope {
        recipient_key_epoch: 2,
        ..e.clone()
    };
    store.create(next, true).await.unwrap();
    store
        .accept_receipt(e.key(), TransportState::RecipientStored, receipt(&e))
        .await
        .unwrap();
    assert!(
        store
            .list_pending("local", "khive", "local-device", i64::MAX, 10)
            .await
            .unwrap()
            .is_empty(),
        "late receipt must stop logical message retries"
    );
}

#[tokio::test]
async fn malformed_agent_and_oversized_envelope_refused() {
    let (backend, e) = fixture();
    let store = SenderTransportStore::new(backend.pool_arc());
    let mut bad = e.clone();
    bad.sender_agent_id = "lambda:sender".into();
    assert!(store.create(bad, false).await.is_err());
    let mut bad = e.clone();
    bad.enc.pop();
    assert!(store.create(bad, false).await.is_err());
    let mut bad = e;
    bad.ciphertext = vec![0; 65_537];
    assert!(store.create(bad, false).await.is_err());
}

#[tokio::test]
async fn device_replacement_uses_local_sequence_and_same_device_epoch_increases() {
    let (backend, mut a) = fixture();
    a.recipient_key_epoch = 2;
    let store = SenderTransportStore::new(backend.pool_arc());
    store.create(a.clone(), false).await.unwrap();
    store
        .hold(a.key(), Some(HoldReason::RecipientKeyChanged))
        .await
        .unwrap();
    let b = SenderEnvelope {
        recipient_device_id: Uuid::new_v4(),
        recipient_key_epoch: 1,
        ..a.clone()
    };
    store
        .create(b.clone(), true)
        .await
        .expect("new device may start at lower epoch");
    let rows = store
        .list_pending("local", "khive", "local-device", i64::MAX, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "only current local sequence may retry");
    assert_eq!(
        rows[0].envelope, b,
        "new device must be the current envelope"
    );
    assert_eq!(rows[0].envelope_seq, 2);
    store
        .hold(b.key(), Some(HoldReason::RecipientKeyChanged))
        .await
        .unwrap();
    assert!(
        store.create(a.clone(), true).await.is_err(),
        "same device equal epoch must be refused"
    );
    let lower = SenderEnvelope {
        recipient_key_epoch: 1,
        ..a.clone()
    };
    assert!(
        store.create(lower, true).await.is_err(),
        "same device lower epoch must be refused"
    );
    let higher = SenderEnvelope {
        recipient_key_epoch: 3,
        ..a
    };
    store.create(higher.clone(), true).await.unwrap();
    let rows = store
        .list_pending("local", "khive", "local-device", i64::MAX, 10)
        .await
        .unwrap();
    assert_eq!(rows[0].envelope, higher);
    assert_eq!(rows[0].envelope_seq, 3);
}

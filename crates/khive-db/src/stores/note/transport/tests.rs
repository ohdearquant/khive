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
        sender_assurance: SenderAssurance::Claimed,
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
async fn admission_sets_and_replaces_the_resubmission_deadline() {
    let (backend, envelope) = fixture();
    let store = SenderTransportStore::new(backend.pool_arc());
    store.create(envelope.clone(), false).await.unwrap();

    let admitted_at = 10_000_000;
    let deadline = admitted_at + 600_000_000;
    store
        .record_admission(envelope.key(), admitted_at)
        .await
        .unwrap();
    let record = store.get(envelope.key()).await.unwrap().unwrap();
    assert_eq!(record.admitted_at, Some(admitted_at));
    assert_eq!(record.next_retry_at, Some(deadline));
    assert!(store
        .list_pending("local", "khive", "local-device", deadline - 1, 10)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .list_pending("local", "khive", "local-device", deadline, 10)
            .await
            .unwrap()
            .len(),
        1
    );

    let readmitted_at = 20_000_000;
    let readmitted_deadline = readmitted_at + 600_000_000;
    store
        .record_admission(envelope.key(), readmitted_at)
        .await
        .unwrap();
    let readmitted = store.get(envelope.key()).await.unwrap().unwrap();
    assert_eq!(readmitted.admitted_at, Some(readmitted_at));
    assert_eq!(readmitted.next_retry_at, Some(readmitted_deadline));
    assert!(store
        .list_pending("local", "khive", "local-device", deadline, 10)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .list_pending("local", "khive", "local-device", readmitted_deadline, 10)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn admission_time_survives_reopening_the_store() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sender-transport.sqlite3");
    let admitted_at = 900_000_000;
    let deadline = admitted_at + 600_000_000;
    let envelope = {
        let backend = crate::StorageBackend::sqlite(&path).unwrap();
        crate::run_migrations(backend.pool().writer().unwrap().conn_mut()).unwrap();
        let store = SenderTransportStore::new(backend.pool_arc());
        let envelope = fixture().1;
        store.create(envelope.clone(), false).await.unwrap();
        store
            .record_admission(envelope.key(), admitted_at)
            .await
            .unwrap();
        drop(store);
        drop(backend);
        envelope
    };

    let reopened = crate::StorageBackend::sqlite(&path).unwrap();
    crate::run_migrations(reopened.pool().writer().unwrap().conn_mut()).unwrap();
    let store = SenderTransportStore::new(reopened.pool_arc());
    let record = store.get(envelope.key()).await.unwrap().unwrap();
    assert_eq!(record.admitted_at, Some(admitted_at));
    assert_eq!(record.next_retry_at, Some(deadline));
    assert!(store
        .list_pending("local", "khive", "local-device", deadline - 1, 10)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .list_pending("local", "khive", "local-device", deadline, 10)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn admission_refuses_held_and_receipted_rows_without_mutation() {
    let (backend, held_envelope) = fixture();
    let held_store = SenderTransportStore::new(backend.pool_arc());
    held_store
        .create(held_envelope.clone(), false)
        .await
        .unwrap();
    held_store
        .record_admission(held_envelope.key(), 10_000_000)
        .await
        .unwrap();
    held_store
        .hold(held_envelope.key(), Some(HoldReason::InsufficientCredit))
        .await
        .unwrap();
    let held_before = held_store.get(held_envelope.key()).await.unwrap().unwrap();
    assert!(held_store
        .record_admission(held_envelope.key(), 30_000_000)
        .await
        .is_err());
    assert_eq!(
        held_store.get(held_envelope.key()).await.unwrap(),
        Some(held_before)
    );

    let (backend, receipted_envelope) = fixture();
    let receipted_store = SenderTransportStore::new(backend.pool_arc());
    receipted_store
        .create(receipted_envelope.clone(), false)
        .await
        .unwrap();
    receipted_store
        .record_admission(receipted_envelope.key(), 10_000_000)
        .await
        .unwrap();
    receipted_store
        .accept_receipt(
            receipted_envelope.key(),
            TransportState::RecipientStored,
            serde_json::json!({
                "binding": {
                    "protocol_version": receipted_envelope.protocol_version,
                    "logical_message_id": receipted_envelope.logical_message_id,
                    "sender_agent_id": receipted_envelope.sender_agent_id,
                    "recipient_agent_id": receipted_envelope.recipient_agent_id,
                    "recipient_device_id": receipted_envelope.recipient_device_id,
                    "recipient_key_epoch": receipted_envelope.recipient_key_epoch,
                    "contact_generation": receipted_envelope.contact_generation,
                    "delivery_attempt_id": Uuid::new_v4(),
                },
                "disposition": "stored",
                "signature": "test"
            }),
        )
        .await
        .unwrap();
    let receipted_before = receipted_store
        .get(receipted_envelope.key())
        .await
        .unwrap()
        .unwrap();
    assert!(receipted_store
        .record_admission(receipted_envelope.key(), 30_000_000)
        .await
        .is_err());
    assert_eq!(
        receipted_store.get(receipted_envelope.key()).await.unwrap(),
        Some(receipted_before)
    );
    assert!(receipted_store
        .list_pending("local", "khive", "local-device", i64::MAX, 10)
        .await
        .unwrap()
        .is_empty());
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
async fn policy_denied_hold_preserves_retry_state_and_bytes() {
    let (backend, envelope) = fixture();
    let store = SenderTransportStore::new(backend.pool_arc());
    store.create(envelope.clone(), false).await.unwrap();
    store
        .record_failure(envelope.key(), FailureClass::Transient, Some(42))
        .await
        .unwrap();
    let before = store.get(envelope.key()).await.unwrap().unwrap();
    store
        .hold(
            envelope.key(),
            Some(HoldReason::PolicyDenied {
                mode: PolicyMode::Enforce,
                revision: 7,
            }),
        )
        .await
        .expect("policy_denied hold must be accepted");
    let held = store.get(envelope.key()).await.unwrap().unwrap();
    assert_eq!(
        held.hold_reason,
        Some(HoldReason::PolicyDenied {
            mode: PolicyMode::Enforce,
            revision: 7
        })
    );
    assert!(
        store
            .list_pending("local", "khive", "local-device", i64::MAX, 10)
            .await
            .unwrap()
            .is_empty(),
        "policy_denied hold must suppress pending delivery"
    );
    store
        .hold(envelope.key(), None)
        .await
        .expect("release must clear both policy columns");
    let released = store.get(envelope.key()).await.unwrap().unwrap();
    assert_eq!(released.hold_reason, None);
    let columns: (Option<String>, Option<i64>) = backend
        .pool()
        .writer()
        .unwrap()
        .conn()
        .query_row(
            "SELECT policy_mode, policy_revision FROM comm_sender_transport",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        columns,
        (None, None),
        "release must clear both policy columns"
    );
    let pending = store
        .list_pending("local", "khive", "local-device", i64::MAX, 10)
        .await
        .unwrap();
    assert_eq!(pending, vec![released.clone()]);
    for row in [held, released] {
        assert_eq!(row.envelope, before.envelope);
        assert_eq!(row.state, before.state);
        assert_eq!(row.attempt_count, before.attempt_count);
        assert_eq!(row.next_retry_at, before.next_retry_at);
        assert_eq!(row.last_failure_class, before.last_failure_class);
        assert_eq!(row.envelope_seq, before.envelope_seq);
        assert_eq!(row.receipt, before.receipt);
    }
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

async fn assert_policy_columns_rejected(
    hold: Option<&str>,
    mode: Option<&str>,
    revision: Option<i64>,
) {
    let (backend, envelope) = fixture();
    SenderTransportStore::new(backend.pool_arc())
        .create(envelope, false)
        .await
        .unwrap();
    let writer = backend.pool().writer().unwrap();
    let conn = writer.conn();
    // Prove this statement and these column names work before testing invalid combinations.
    let update =
        "UPDATE comm_sender_transport SET hold_reason=?1,policy_mode=?2,policy_revision=?3";
    assert_eq!(
        conn.execute(update, params!["policy_denied", "enforce", 7])
            .unwrap(),
        1
    );
    let error = conn
        .execute(update, params![hold, mode, revision])
        .expect_err("inconsistent policy columns must fail the table CHECK");
    match error {
        rusqlite::Error::SqliteFailure(code, _) => {
            assert_eq!(code.extended_code, rusqlite::ffi::SQLITE_CONSTRAINT_CHECK)
        }
        other => panic!("expected CHECK constraint failure, got {other}"),
    }
}

#[tokio::test]
async fn policy_columns_reject_missing_mode() {
    assert_policy_columns_rejected(Some("policy_denied"), None, Some(7)).await;
}
#[tokio::test]
async fn policy_columns_reject_missing_revision() {
    assert_policy_columns_rejected(Some("policy_denied"), Some("enforce"), None).await;
}
#[tokio::test]
async fn policy_columns_reject_mode_on_other_hold() {
    for hold in [
        Some("insufficient_credit"),
        Some("recipient_key_changed"),
        None,
    ] {
        assert_policy_columns_rejected(hold, Some("enforce"), None).await;
    }
}
#[tokio::test]
async fn policy_columns_reject_revision_on_other_hold() {
    for hold in [
        Some("insufficient_credit"),
        Some("recipient_key_changed"),
        None,
    ] {
        assert_policy_columns_rejected(hold, None, Some(7)).await;
    }
}
#[tokio::test]
async fn policy_columns_reject_invalid_mode_and_negative_revision() {
    assert_policy_columns_rejected(Some("policy_denied"), Some("invalid"), Some(7)).await;
    assert_policy_columns_rejected(Some("policy_denied"), Some("enforce"), Some(-1)).await;
}

#[tokio::test]
async fn sender_assurance_round_trips_and_exact_retry_compares_it() {
    for (assurance, spelling) in [
        (SenderAssurance::Claimed, "claimed"),
        (SenderAssurance::DaemonBearer, "daemon_bearer"),
        (SenderAssurance::ActorSignature, "actor_signature"),
    ] {
        let (backend, mut envelope) = fixture();
        envelope.sender_assurance = assurance;
        let store = SenderTransportStore::new(backend.pool_arc());
        let first = store.create(envelope.clone(), false).await.unwrap();
        assert_eq!(first.envelope, envelope);
        assert_eq!(
            store.get(envelope.key()).await.unwrap(),
            Some(first.clone())
        );
        assert_eq!(store.create(envelope.clone(), false).await.unwrap(), first);
        let stored: String = backend
            .pool()
            .writer()
            .unwrap()
            .conn()
            .query_row(
                "SELECT sender_assurance FROM comm_sender_transport",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, spelling);
        let mut json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(json["sender_assurance"], spelling);
        assert_eq!(
            serde_json::from_value::<SenderEnvelope>(json.clone()).unwrap(),
            envelope
        );
        json.as_object_mut().unwrap().remove("sender_assurance");
        assert!(serde_json::from_value::<SenderEnvelope>(json).is_err());
        for different in [
            SenderAssurance::Claimed,
            SenderAssurance::DaemonBearer,
            SenderAssurance::ActorSignature,
        ] {
            if different == assurance {
                continue;
            }
            let mut changed = envelope.clone();
            changed.sender_assurance = different;
            let error = store.create(changed, false).await.unwrap_err();
            assert!(
                matches!(
                    &error,
                    StorageError::WriterTaskRequestFailed {
                        request_state: khive_storage::WriterTaskRequestState::TransactionRolledBack,
                        source,
                    } if matches!(source.as_ref(), StorageError::InvalidInput { message, .. }
                        if message == "envelope_conflict")
                ),
                "expected rolled-back envelope_conflict; got {error:?}"
            );
            assert_eq!(
                store.get(envelope.key()).await.unwrap(),
                Some(first.clone())
            );
        }
    }
}

#[tokio::test]
async fn confirmed_reencryption_preserves_sender_assurance() {
    for assurance in [
        SenderAssurance::Claimed,
        SenderAssurance::DaemonBearer,
        SenderAssurance::ActorSignature,
    ] {
        let (backend, mut envelope) = fixture();
        envelope.sender_assurance = assurance;
        let store = SenderTransportStore::new(backend.pool_arc());
        store.create(envelope.clone(), false).await.unwrap();
        store
            .hold(envelope.key(), Some(HoldReason::RecipientKeyChanged))
            .await
            .unwrap();
        let prior = store.get(envelope.key()).await.unwrap().unwrap();
        let next = SenderEnvelope {
            recipient_key_epoch: 2,
            enc: vec![3; 32],
            ..envelope.clone()
        };
        for different in [
            SenderAssurance::Claimed,
            SenderAssurance::DaemonBearer,
            SenderAssurance::ActorSignature,
        ] {
            if different == assurance {
                continue;
            }
            let mut changed = next.clone();
            changed.sender_assurance = different;
            let error = store
                .create(changed, true)
                .await
                .expect_err("re-encryption must refuse changed sender assurance");
            assert!(
                matches!(
                    &error,
                    StorageError::WriterTaskRequestFailed {
                        request_state: khive_storage::WriterTaskRequestState::TransactionRolledBack,
                        source,
                    } if matches!(source.as_ref(), StorageError::InvalidInput { message, .. }
                        if message == "sender_assurance_conflict")
                ),
                "expected rolled-back sender_assurance_conflict; got {error:?}"
            );
            assert!(store.get(next.key()).await.unwrap().is_none());
            assert_eq!(
                store.get(envelope.key()).await.unwrap(),
                Some(prior.clone())
            );
            let count: i64 = backend
                .pool()
                .writer()
                .unwrap()
                .conn()
                .query_row("SELECT count(*) FROM comm_sender_transport", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 1, "refusal must not write a row");
        }
        let created = store.create(next.clone(), true).await.unwrap();
        assert_eq!(created.envelope, next);
        assert_eq!(created.envelope_seq, 2);
        assert_eq!(store.get(next.key()).await.unwrap(), Some(created));
        assert_eq!(store.get(envelope.key()).await.unwrap(), Some(prior));
    }
}

async fn assert_sender_assurance_insert_rejected(value: Option<&str>, code: i32) {
    let (backend, envelope) = fixture();
    SenderTransportStore::new(backend.pool_arc())
        .create(envelope, false)
        .await
        .unwrap();
    let writer = backend.pool().writer().unwrap();
    let conn = writer.conn();
    let selected = COLUMNS
        .split(',')
        .map(|column| match column.trim() {
            "logical_message_id" => "?1",
            "sender_assurance" => "?2",
            other => other,
        })
        .collect::<Vec<_>>()
        .join(",");
    let insert = format!(
        "INSERT INTO comm_sender_transport ({COLUMNS}) \
        SELECT {selected} FROM comm_sender_transport LIMIT 1"
    );
    for spelling in ["claimed", "daemon_bearer", "actor_signature"] {
        assert_eq!(
            conn.execute(&insert, params![Uuid::new_v4().to_string(), spelling])
                .unwrap(),
            1
        );
    }
    let error = conn
        .execute(&insert, params![Uuid::new_v4().to_string(), value])
        .expect_err("invalid sender assurance must be refused by SQL");
    match error {
        rusqlite::Error::SqliteFailure(error, _) => assert_eq!(error.extended_code, code),
        other => panic!("expected constraint failure, got {other}"),
    }
    let count: i64 = conn
        .query_row("SELECT count(*) FROM comm_sender_transport", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 4);
}

#[tokio::test]
async fn sender_assurance_insert_rejects_null() {
    assert_sender_assurance_insert_rejected(None, rusqlite::ffi::SQLITE_CONSTRAINT_NOTNULL).await;
}

#[tokio::test]
async fn sender_assurance_insert_rejects_unknown_spelling() {
    assert_sender_assurance_insert_rejected(
        Some("unspecified"),
        rusqlite::ffi::SQLITE_CONSTRAINT_CHECK,
    )
    .await;
}

#[tokio::test]
async fn confirmed_reencryption_rejects_sender_key_epoch_change() {
    let (backend, envelope) = fixture();
    let store = SenderTransportStore::new(backend.pool_arc());
    store.create(envelope.clone(), false).await.unwrap();
    store
        .hold(envelope.key(), Some(HoldReason::RecipientKeyChanged))
        .await
        .unwrap();
    let prior = store.get(envelope.key()).await.unwrap().unwrap();
    let mut next = SenderEnvelope {
        recipient_key_epoch: 2,
        sender_key_epoch: 2,
        ..envelope.clone()
    };
    let error = store
        .create(next.clone(), true)
        .await
        .expect_err("re-encryption must refuse changed sender key epoch");
    assert!(
        matches!(&error, StorageError::WriterTaskRequestFailed { source, .. }
        if matches!(source.as_ref(), StorageError::InvalidInput { message, .. }
            if message == "logical message identity cannot change")),
        "unexpected error: {error:?}"
    );
    assert!(store.get(next.key()).await.unwrap().is_none());
    assert_eq!(
        store.get(envelope.key()).await.unwrap(),
        Some(prior.clone())
    );
    next.sender_key_epoch = envelope.sender_key_epoch;
    assert_eq!(
        store.create(next.clone(), true).await.unwrap().envelope,
        next
    );
    assert_eq!(store.get(envelope.key()).await.unwrap(), Some(prior));
}

fn assert_held_failure_refusal(error: &StorageError, marker: &str) {
    let cause = match error {
        StorageError::WriterTaskRequestFailed { source, .. } => source.as_ref(),
        direct => direct,
    };
    assert!(
        matches!(cause, StorageError::InvalidInput { message, .. }
            if message == "sender record is held"),
        "{marker}: expected the held sender refusal, got {error:?}"
    );
}

#[tokio::test]
async fn held_transport_refuses_every_failure_class_without_mutation() {
    for hold in [
        HoldReason::InsufficientCredit,
        HoldReason::RecipientKeyChanged,
        HoldReason::PolicyDenied {
            mode: PolicyMode::Enforce,
            revision: 7,
        },
    ] {
        for (class, refusal_marker) in [
            (
                FailureClass::Permanent,
                "held_permanent_failure_must_refuse",
            ),
            (
                FailureClass::Transient,
                "held_transient_failure_must_refuse",
            ),
            (
                FailureClass::Authentication,
                "held_authentication_failure_must_refuse",
            ),
        ] {
            let (backend, envelope) = fixture();
            let store = SenderTransportStore::new(backend.pool_arc());
            store.create(envelope.clone(), false).await.unwrap();
            for retry_at in [21, 42] {
                store
                    .record_failure(envelope.key(), FailureClass::Transient, Some(retry_at))
                    .await
                    .unwrap();
            }
            store.hold(envelope.key(), Some(hold)).await.unwrap();
            assert_eq!(
                backend
                    .pool()
                    .writer()
                    .unwrap()
                    .conn()
                    .execute("UPDATE comm_sender_transport SET updated_at=17", [])
                    .unwrap(),
                1
            );
            let policy_columns = || -> (Option<String>, Option<i64>) {
                backend
                    .pool()
                    .writer()
                    .unwrap()
                    .conn()
                    .query_row(
                        "SELECT policy_mode, policy_revision FROM comm_sender_transport",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .unwrap()
            };
            let before = store.get(envelope.key()).await.unwrap().unwrap();
            let policy_before = policy_columns();
            assert_eq!(before.state, TransportState::Pending);
            assert_eq!(before.hold_reason, Some(hold));
            assert_eq!(before.attempt_count, 2);
            assert_eq!(before.next_retry_at, Some(42));
            assert_eq!(before.last_failure_class, Some(FailureClass::Transient));
            assert_eq!(before.updated_at, 17);
            assert_eq!(
                policy_before,
                match hold {
                    HoldReason::PolicyDenied { .. } => (Some("enforce".to_string()), Some(7)),
                    _ => (None, None),
                }
            );

            let error = store
                .record_failure(envelope.key(), class, Some(84))
                .await
                .expect_err(refusal_marker);
            assert_held_failure_refusal(&error, refusal_marker);
            assert_eq!(
                store.get(envelope.key()).await.unwrap(),
                Some(before),
                "held_failure_record_must_remain_unchanged: hold={hold:?}, class={class:?}"
            );
            assert_eq!(
                policy_columns(),
                policy_before,
                "held_failure_policy_columns_must_remain_unchanged: hold={hold:?}, class={class:?}"
            );
        }
    }
}

#[tokio::test]
async fn recipient_key_change_recovers_after_permanent_failure_refusal() {
    let (backend, envelope) = fixture();
    let store = SenderTransportStore::new(backend.pool_arc());
    store.create(envelope.clone(), false).await.unwrap();
    store
        .record_failure(envelope.key(), FailureClass::Transient, Some(42))
        .await
        .unwrap();
    store
        .hold(envelope.key(), Some(HoldReason::RecipientKeyChanged))
        .await
        .unwrap();
    let held = store.get(envelope.key()).await.unwrap().unwrap();
    let error = store
        .record_failure(envelope.key(), FailureClass::Permanent, None)
        .await
        .expect_err("key_change_permanent_failure_must_refuse");
    assert_held_failure_refusal(&error, "key_change_permanent_failure_must_refuse");
    assert_eq!(store.get(envelope.key()).await.unwrap(), Some(held.clone()));

    let next = SenderEnvelope {
        recipient_key_epoch: envelope.recipient_key_epoch + 1,
        recipient_key_fingerprint: "cd".repeat(32),
        enc: vec![3; 32],
        ciphertext: vec![4, 0, 255],
        ..envelope.clone()
    };
    assert!(
        store.create(next.clone(), false).await.is_err(),
        "key_change_recovery_still_requires_confirmation"
    );
    let recovered = store
        .create(next.clone(), true)
        .await
        .expect("key_change_confirmed_reencryption_must_survive_failure_refusal");
    assert_eq!(recovered.envelope, next);
    assert_eq!(recovered.state, TransportState::Pending);
    assert_eq!(recovered.hold_reason, None);
    assert_eq!(recovered.envelope_seq, held.envelope_seq + 1);
    assert_eq!(recovered.attempt_count, 0);
    assert_eq!(recovered.next_retry_at, None);
    assert_eq!(recovered.last_failure_class, None);
    assert_eq!(store.get(envelope.key()).await.unwrap(), Some(held));
    assert_eq!(
        store
            .list_pending("local", "khive", "local-device", i64::MAX, 10)
            .await
            .unwrap(),
        vec![recovered],
        "key_change_recovery_must_make_only_the_new_envelope_retryable"
    );
}

#[tokio::test]
async fn non_pending_transport_cannot_hold() {
    let (backend, envelope) = fixture();
    SenderTransportStore::new(backend.pool_arc())
        .create(envelope, false)
        .await
        .unwrap();
    let writer = backend.pool().writer().unwrap();
    let conn = writer.conn();
    let update = "UPDATE comm_sender_transport SET state=?1,hold_reason=?2";
    for state in ["failed", "recipient_stored", "recipient_quarantined"] {
        assert_eq!(
            conn.execute(update, params![state, Option::<String>::None])
                .unwrap(),
            1
        );
        assert_eq!(
            conn.execute(update, params!["pending", "insufficient_credit"])
                .unwrap(),
            1
        );
        let error = conn
            .execute(update, params![state, "insufficient_credit"])
            .expect_err("non-pending hold must fail the table CHECK");
        match error {
            rusqlite::Error::SqliteFailure(code, _) => {
                assert_eq!(code.extended_code, rusqlite::ffi::SQLITE_CONSTRAINT_CHECK)
            }
            other => panic!("expected CHECK constraint failure, got {other}"),
        }
    }
}

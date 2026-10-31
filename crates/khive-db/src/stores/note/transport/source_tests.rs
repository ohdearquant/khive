//! `SenderTransportStore::current_by_logical_id`: the current envelope of a logical message is
//! the row with the highest `envelope_seq`, whatever its state, and only on the exact route.
use super::*;
use crate::StorageBackend;

const NAMESPACE: &str = "local";
const KIND: &str = "khive";
const SLUG: &str = "local-device";

fn first_envelope() -> SenderEnvelope {
    SenderEnvelope {
        namespace: NAMESPACE.into(),
        logical_message_id: Uuid::new_v4(),
        outbound_note_id: Uuid::new_v4(),
        kind: KIND.into(),
        slug: SLUG.into(),
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
    }
}

/// The re-encrypted envelope for the same device at the next recipient key epoch.
fn second_envelope(first: &SenderEnvelope) -> SenderEnvelope {
    SenderEnvelope {
        recipient_key_epoch: first.recipient_key_epoch + 1,
        recipient_key_fingerprint: "cd".repeat(32),
        ciphertext: vec![3, 4, 5],
        ..first.clone()
    }
}

fn open_store() -> (StorageBackend, SenderTransportStore) {
    let backend = StorageBackend::memory().unwrap();
    backend.pool().run_migrations().unwrap();
    let store = SenderTransportStore::new(backend.pool_arc());
    (backend, store)
}

/// Two envelopes of one logical message: sequence 1 held for a key change, then the confirmed
/// re-encryption at sequence 2.
async fn two_envelopes(store: &SenderTransportStore) -> (SenderEnvelope, SenderEnvelope) {
    let first = first_envelope();
    store.create(first.clone(), false).await.unwrap();
    store
        .hold(first.key(), Some(HoldReason::RecipientKeyChanged))
        .await
        .unwrap();
    let second = second_envelope(&first);
    let created = store.create(second.clone(), true).await.unwrap();
    assert_eq!(created.envelope_seq, 2);
    (first, second)
}

async fn current(store: &SenderTransportStore, envelope: &SenderEnvelope) -> Option<SenderRecord> {
    store
        .current_by_logical_id(NAMESPACE, KIND, SLUG, envelope.logical_message_id)
        .await
        .unwrap()
}

#[tokio::test]
async fn current_by_logical_id_returns_highest_envelope_seq_even_when_held() {
    let (_backend, store) = open_store();
    let (first, second) = two_envelopes(&store).await;

    let top = current(&store, &first).await.expect("a current row");
    assert_eq!(top.envelope_seq, 2);
    assert_eq!(top.envelope.recipient_key_epoch, 2);
    assert_eq!(top.envelope, second);

    // Hold the top row: it stays the current envelope; the unheld lower row must not win.
    store
        .hold(second.key(), Some(HoldReason::InsufficientCredit))
        .await
        .unwrap();
    let held = current(&store, &first)
        .await
        .expect("held row is still current");
    assert_eq!(held.envelope_seq, 2);
    assert_eq!(held.hold_reason, Some(HoldReason::InsufficientCredit));
    assert_eq!(held.envelope.recipient_key_epoch, 2);

    // The historical envelope is still readable by its full key, at its own sequence.
    let historical = store.get(first.key()).await.unwrap().unwrap();
    assert_eq!(historical.envelope_seq, 1);
    assert_eq!(historical.envelope, first);
}

#[tokio::test]
async fn current_by_logical_id_returns_highest_envelope_seq_even_when_failed() {
    let (_backend, store) = open_store();
    let (first, second) = two_envelopes(&store).await;

    store
        .record_failure(second.key(), FailureClass::Permanent, None)
        .await
        .unwrap();
    let failed = current(&store, &first)
        .await
        .expect("failed row is still current");
    assert_eq!(failed.envelope_seq, 2);
    assert_eq!(failed.state, TransportState::Failed);
    assert_eq!(failed.last_failure_class, Some(FailureClass::Permanent));
    assert_eq!(failed.envelope.recipient_key_epoch, 2);
}

#[tokio::test]
async fn current_by_logical_id_excludes_other_namespace_kind_and_slug() {
    let (_backend, store) = open_store();
    let envelope = first_envelope();
    store.create(envelope.clone(), false).await.unwrap();
    let id = envelope.logical_message_id;

    // Control: the exact route finds the row.
    let exact = store
        .current_by_logical_id(NAMESPACE, KIND, SLUG, id)
        .await
        .unwrap()
        .expect("exact route finds the row");
    assert_eq!(exact.envelope, envelope);

    for (namespace, kind, slug) in [
        ("other", KIND, SLUG),
        (NAMESPACE, "other-kind", SLUG),
        (NAMESPACE, KIND, "other-slug"),
    ] {
        assert!(
            store
                .current_by_logical_id(namespace, kind, slug, id)
                .await
                .unwrap()
                .is_none(),
            "{namespace}/{kind}/{slug} must not see a row of {NAMESPACE}/{KIND}/{SLUG}"
        );
    }
}

#[tokio::test]
async fn current_by_logical_id_without_a_row_is_none() {
    let (_backend, store) = open_store();
    let stored = first_envelope();
    let unknown = Uuid::new_v4();

    // Empty database.
    assert!(store
        .current_by_logical_id(NAMESPACE, KIND, SLUG, unknown)
        .await
        .unwrap()
        .is_none());

    // A different logical message exists; it must not be returned for the unknown id.
    store.create(stored.clone(), false).await.unwrap();
    assert!(store
        .current_by_logical_id(NAMESPACE, KIND, SLUG, unknown)
        .await
        .unwrap()
        .is_none());
    assert!(current(&store, &stored).await.is_some());
}

use super::*;
use crate::StorageBackend;

const CONFLICT_TEXT: &str = "node pin identity already has different confirmed keys";

fn bytes(seed: u8) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = seed.wrapping_add(index as u8);
    }
    out
}

fn key(slug: &str, device: u128, epoch: u64) -> NodePinKey {
    NodePinKey {
        namespace: "local".into(),
        slug: slug.into(),
        realm: "example".into(),
        agent_id: Uuid::from_u128(7),
        device_id: Uuid::from_u128(device),
        key_epoch: epoch,
    }
}

fn record(key: NodePinKey, seed: u8) -> NodePinRecord {
    NodePinRecord {
        key,
        kem_public_key: bytes(seed),
        signing_public_key: bytes(seed.wrapping_add(50)),
        fingerprint: bytes(seed.wrapping_add(100)),
    }
}

fn fixture() -> (StorageBackend, NodePinStore) {
    let backend = StorageBackend::memory().unwrap();
    backend.pool().run_migrations().unwrap();
    let store = NodePinStore::new(backend.pool_arc());
    (backend, store)
}

fn is_conflict(error: &StorageError) -> bool {
    matches!(
        error,
        StorageError::WriterTaskRequestFailed {
            request_state: khive_storage::WriterTaskRequestState::TransactionRolledBack,
            source,
        } if matches!(source.as_ref(), StorageError::InvalidInput { message, .. }
            if message == CONFLICT_TEXT)
    )
}

#[tokio::test]
async fn retain_then_get_round_trips_every_byte_across_store_instances() {
    let (backend, store) = fixture();
    let pin = record(key("peer", 1, 1), 1);
    store.retain(vec![pin.clone()]).await.unwrap();
    assert_eq!(store.get(pin.key.clone()).await.unwrap(), Some(pin.clone()));
    let second = NodePinStore::new(backend.pool_arc());
    assert_eq!(second.get(pin.key.clone()).await.unwrap(), Some(pin));
}

#[tokio::test]
async fn get_of_an_unknown_identity_is_absent() {
    let (_backend, store) = fixture();
    assert_eq!(store.get(key("missing", 1, 1)).await.unwrap(), None);
}

#[tokio::test]
async fn retaining_the_same_payload_twice_is_a_no_op() {
    let (_backend, store) = fixture();
    let pin = record(key("peer", 1, 1), 1);
    store.retain(vec![pin.clone()]).await.unwrap();
    store.retain(vec![pin.clone()]).await.unwrap();
    store.retain(vec![pin.clone(), pin.clone()]).await.unwrap();
    assert_eq!(store.get(pin.key.clone()).await.unwrap(), Some(pin));
}

#[tokio::test]
async fn retaining_an_empty_batch_succeeds() {
    let (_backend, store) = fixture();
    store.retain(Vec::new()).await.unwrap();
}

#[tokio::test]
async fn different_bytes_at_an_existing_identity_are_refused_and_never_overwrite() {
    let (_backend, store) = fixture();
    let existing = record(key("peer", 1, 1), 1);
    store.retain(vec![existing.clone()]).await.unwrap();
    let mut kem = existing.clone();
    kem.kem_public_key = bytes(200);
    let mut signing = existing.clone();
    signing.signing_public_key = bytes(201);
    let mut fingerprint = existing.clone();
    fingerprint.fingerprint = bytes(202);
    for changed in [kem, signing, fingerprint] {
        let error = store.retain(vec![changed]).await.unwrap_err();
        assert!(is_conflict(&error), "{error:?}");
        assert_eq!(
            store.get(existing.key.clone()).await.unwrap(),
            Some(existing.clone())
        );
    }
}

#[tokio::test]
async fn a_conflicting_entry_rolls_back_the_earlier_entries_of_the_same_call() {
    let (_backend, store) = fixture();
    let existing = record(key("peer", 1, 1), 1);
    store.retain(vec![existing.clone()]).await.unwrap();
    let fresh = record(key("other", 2, 1), 9);
    let mut changed = existing.clone();
    changed.kem_public_key = bytes(200);
    let error = store
        .retain(vec![fresh.clone(), changed])
        .await
        .unwrap_err();
    assert!(is_conflict(&error), "{error:?}");
    assert_eq!(store.get(fresh.key.clone()).await.unwrap(), None);
    assert_eq!(
        store.get(existing.key.clone()).await.unwrap(),
        Some(existing)
    );
}

#[tokio::test]
async fn an_old_epoch_stays_readable_after_a_newer_epoch_is_retained() {
    let (_backend, store) = fixture();
    let old = record(key("peer", 1, 1), 1);
    let new = record(key("peer", 1, 2), 30);
    store.retain(vec![old.clone()]).await.unwrap();
    store.retain(vec![new.clone()]).await.unwrap();
    assert_eq!(store.get(old.key.clone()).await.unwrap(), Some(old));
    assert_eq!(store.get(new.key.clone()).await.unwrap(), Some(new));
}

#[tokio::test]
async fn epochs_outside_the_stored_range_are_refused_before_anything_is_written() {
    let (_backend, store) = fixture();
    let valid = record(key("peer", 1, 1), 1);
    for epoch in [0u64, u64::from(u32::MAX) + 1, u64::MAX] {
        let bad = record(key("peer", 1, epoch), 5);
        let error = store
            .retain(vec![valid.clone(), bad.clone()])
            .await
            .unwrap_err();
        assert!(
            matches!(error, StorageError::InvalidInput { .. }),
            "{error:?}"
        );
        assert_eq!(store.get(valid.key.clone()).await.unwrap(), None);
        assert_eq!(store.get(bad.key.clone()).await.unwrap(), None);
    }
    let highest = record(key("peer", 1, u64::from(u32::MAX)), 6);
    store.retain(vec![highest.clone()]).await.unwrap();
    assert_eq!(store.get(highest.key.clone()).await.unwrap(), Some(highest));
}

#[tokio::test]
async fn identities_differing_in_one_component_are_distinct() {
    let (_backend, store) = fixture();
    let base = key("peer", 1, 1);
    let mut keys = vec![base.clone()];
    let mut other = base.clone();
    other.namespace = "other".into();
    keys.push(other);
    let mut other = base.clone();
    other.slug = "other".into();
    keys.push(other);
    let mut other = base.clone();
    other.realm = "other".into();
    keys.push(other);
    let mut other = base.clone();
    other.agent_id = Uuid::from_u128(8);
    keys.push(other);
    let mut other = base.clone();
    other.device_id = Uuid::from_u128(2);
    keys.push(other);
    let mut other = base.clone();
    other.key_epoch = 2;
    keys.push(other);
    let records: Vec<NodePinRecord> = keys
        .into_iter()
        .enumerate()
        .map(|(index, key)| record(key, (index as u8) * 10 + 1))
        .collect();
    store.retain(records.clone()).await.unwrap();
    for pin in &records {
        assert_eq!(
            store.get(pin.key.clone()).await.unwrap().as_ref(),
            Some(pin)
        );
    }
}

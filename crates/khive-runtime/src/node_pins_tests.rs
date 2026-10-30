use super::*;
use crate::error::RuntimeError;
use khive_storage::StorageError;
use uuid::Uuid;

const CONFLICT_TEXT: &str = "node pin identity already has different confirmed keys";

fn bytes(seed: u8) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = seed.wrapping_add(index as u8);
    }
    out
}

fn key(slug: &str, epoch: u64) -> NodePinKey {
    NodePinKey {
        namespace: "local".into(),
        slug: slug.into(),
        realm: "example".into(),
        agent_id: Uuid::from_u128(7),
        device_id: Uuid::from_u128(1),
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

#[tokio::test]
async fn retained_pin_round_trips_through_the_runtime() {
    let runtime = KhiveRuntime::memory().unwrap();
    let pin = record(key("peer", 1), 1);
    runtime
        .retain_confirmed_node_pins(vec![pin.clone()])
        .await
        .unwrap();
    runtime
        .retain_confirmed_node_pins(vec![pin.clone()])
        .await
        .unwrap();
    assert_eq!(
        runtime.confirmed_node_pin(pin.key.clone()).await.unwrap(),
        Some(pin)
    );
    assert_eq!(
        runtime.confirmed_node_pin(key("missing", 1)).await.unwrap(),
        None
    );
}

#[tokio::test]
async fn a_conflicting_pin_is_refused_with_a_typed_storage_error_and_rolls_back() {
    let runtime = KhiveRuntime::memory().unwrap();
    let existing = record(key("peer", 1), 1);
    runtime
        .retain_confirmed_node_pins(vec![existing.clone()])
        .await
        .unwrap();
    let fresh = record(key("other", 1), 9);
    let mut changed = existing.clone();
    changed.signing_public_key = bytes(200);
    let error = runtime
        .retain_confirmed_node_pins(vec![fresh.clone(), changed])
        .await
        .unwrap_err();
    assert!(
        matches!(
            &error,
            RuntimeError::Storage(StorageError::InvalidInput { message, .. })
                if message == CONFLICT_TEXT
        ),
        "{error:?}"
    );
    assert_eq!(runtime.confirmed_node_pin(fresh.key).await.unwrap(), None);
    assert_eq!(
        runtime
            .confirmed_node_pin(existing.key.clone())
            .await
            .unwrap(),
        Some(existing)
    );
}

#[tokio::test]
async fn an_old_epoch_stays_readable_and_pins_stay_with_their_own_runtime() {
    let runtime = KhiveRuntime::memory().unwrap();
    let unrelated = KhiveRuntime::memory().unwrap();
    let old = record(key("peer", 1), 1);
    let new = record(key("peer", 2), 30);
    runtime
        .retain_confirmed_node_pins(vec![old.clone()])
        .await
        .unwrap();
    runtime
        .retain_confirmed_node_pins(vec![new.clone()])
        .await
        .unwrap();
    assert_eq!(
        runtime.confirmed_node_pin(old.key.clone()).await.unwrap(),
        Some(old.clone())
    );
    assert_eq!(
        runtime.confirmed_node_pin(new.key.clone()).await.unwrap(),
        Some(new)
    );
    assert_eq!(unrelated.confirmed_node_pin(old.key).await.unwrap(), None);
}

#![cfg(not(feature = "serde"))]

use khive_fold::{Checkpoint, CheckpointStore, FoldContext, InMemoryCheckpointStore};
use khive_types::Hash32;
use uuid::Uuid;

#[test]
fn checkpoint_api_remains_available_without_serde_derives() {
    let checkpoint = Checkpoint::new(
        "no-default:one",
        "serializable state".to_string(),
        Uuid::from_u128(1),
        1,
        FoldContext::new(),
        1,
    )
    .expect("checkpoint hashes a serializable state without derives");
    let expected_hash = Hash32::from_blake3(
        &serde_json::to_vec("serializable state").expect("serialize fixture state"),
    );
    assert_eq!(checkpoint.hash, expected_hash);
    let store = InMemoryCheckpointStore::new();
    store.save(checkpoint).expect("save checkpoint");
    let loaded = store
        .load("no-default:one")
        .expect("verify checkpoint")
        .expect("checkpoint exists");
    assert_eq!(loaded.state, "serializable state");
}

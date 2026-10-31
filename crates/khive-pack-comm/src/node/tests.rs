//! Tests for the node module of the comm pack (wave 2b).
//!
//! STATUS: written without compiling or running (the leg brief forbids cargo). Every API used
//! here was read from the tree, but nothing has been type-checked.

use khive_channel_node::encoding::{CanonicalUuid, Epoch, HexBytes, Realm};
use khive_channel_node::keys::{InMemoryKeyFacility, KeyFacility};
use khive_channel_node::pins::{ConfirmedPin, PinIdentity};
use khive_channel_node::ProtocolError;
use uuid::Uuid;

// Kills: dropping the fingerprint check in `ConfirmedPin::new`
// (`khive-channel-node/src/pins.rs`, `if keys.fingerprint() != fingerprint {` -> `if false {`).
// A pin whose fingerprint does not match its keys must not be constructible, and the only
// constructor is `ConfirmedPin::new` (the fields are private).
#[test]
fn confirmed_pin_is_built_only_through_confirmed_pin_new() {
    let facility = InMemoryKeyFacility::generate().expect("generate in-memory keys");
    let keys = facility.public_keys();
    let identity = PinIdentity {
        realm: Realm::parse("realm.test").expect("realm"),
        agent: CanonicalUuid::from_uuid(Uuid::from_u128(1)),
        device: CanonicalUuid::from_uuid(Uuid::from_u128(2)),
        epoch: Epoch::new(1).expect("epoch"),
    };
    let good = keys.fingerprint();

    let pin = ConfirmedPin::new(identity.clone(), keys.clone(), good.clone())
        .expect("a matching fingerprint confirms the pin");
    assert_eq!(
        pin,
        ConfirmedPin::new(identity.clone(), keys.clone(), good.clone()).expect("same inputs")
    );

    let wrong = HexBytes::new([0u8; 32]);
    assert_ne!(wrong, good, "fixture: the wrong fingerprint must differ");
    let refused = ConfirmedPin::new(identity, keys, wrong);
    assert!(matches!(refused, Err(ProtocolError::InvalidKey)));
}

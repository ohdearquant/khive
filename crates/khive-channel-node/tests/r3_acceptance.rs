//! Acceptance tests for four wire-protocol requirements (ADR-105 Appendix A), written against the
//! public API before the fix. Each one fails on the current models and must pass after it.

use khive_channel::ReceiptDisposition;
use khive_channel_node::encoding::{CanonicalUuid, Epoch, ProtocolVersion};
use khive_channel_node::keys::{
    EnrolmentBundle, InMemoryKeyFacility, KeyFacility, SigningPublicKey,
};
use khive_channel_node::plaintext::Plaintext;
use khive_channel_node::receipt::{WireReceipt, WireReceiptBinding};
use khive_channel_node::request::RequestHeaders;
use khive_channel_node::wire::{AdmissionResponse, PollResponse};
use serde_json::{json, Value};

const ATTEMPT: &str = "01920000-0000-7000-8000-0000000e0001";

// ---- server timestamps: any RFC 3339 offset, normalised to UTC (A.6.2, A.6.3) ----

#[test]
fn server_timestamps_accept_an_offset_and_normalise_to_utc() {
    let admission = json!({
        "state": "pending",
        "delivery_attempt_id": ATTEMPT,
        "admitted_at": "2026-09-23T16:00:00-04:00",
    });
    let parsed: AdmissionResponse = serde_json::from_value(admission)
        .expect("a 202 body whose admitted_at carries an offset must parse");
    let written = serde_json::to_value(&parsed).expect("serialize");
    assert_eq!(written["admitted_at"], "2026-09-23T20:00:00Z");

    let page = json!({
        "deliveries": [],
        "receipts": [],
        "receipts_cursor": 41,
        "server_time": "2026-09-23T21:30:00+05:30",
    });
    let parsed: PollResponse = serde_json::from_value(page)
        .expect("a poll page whose server_time carries an offset must parse");
    let written = serde_json::to_value(&parsed).expect("serialize");
    assert_eq!(written["server_time"], "2026-09-23T16:00:00Z");
}

// ---- Plaintext is constructed only through classification (A.5) ----

struct Probe<T>(std::marker::PhantomData<T>);
trait ViaDeserialize {
    fn deserializable(&self) -> bool {
        true
    }
}
impl<T: serde::de::DeserializeOwned> ViaDeserialize for &Probe<T> {}
trait ViaNothing {
    fn deserializable(&self) -> bool {
        false
    }
}
impl<T> ViaNothing for Probe<T> {}

#[test]
// The double reference is the probe: method resolution reaches the `&Probe<T>` impl only
// through autoref, so removing a `&` would change what the assertion tests.
#[allow(clippy::needless_borrow)]
fn plaintext_has_no_public_deserialize() {
    // Method resolution picks the `&Probe<T>` impl only when `T: DeserializeOwned` holds.
    assert!(
        !(&&Probe::<Plaintext>(std::marker::PhantomData)).deserializable(),
        "Plaintext must not be deserializable outside classify_plaintext"
    );
    // control: the probe does report a type that is deserializable
    assert!((&&Probe::<AdmissionResponse>(std::marker::PhantomData)).deserializable());
}

// ---- a malformed receipt does not make the poll page unreadable (A.6.3, A.8, A.11) ----

fn signed_receipt_item(facility: &InMemoryKeyFacility, seq: u64, attempt: &str) -> Value {
    let binding = WireReceiptBinding {
        protocol_version: ProtocolVersion::new(1).expect("version"),
        logical_message_id: CanonicalUuid::parse("01920000-0000-7000-8000-00000000a001")
            .expect("id"),
        sender_agent_id: CanonicalUuid::parse("01920000-0000-7000-8000-00000000b001").expect("id"),
        recipient_agent_id: CanonicalUuid::parse("01920000-0000-7000-8000-00000000c001")
            .expect("id"),
        recipient_device_id: CanonicalUuid::parse("01920000-0000-7000-8000-00000000d001")
            .expect("id"),
        recipient_key_epoch: Epoch::new(2).expect("epoch"),
        contact_generation: Epoch::new(3).expect("epoch"),
        delivery_attempt_id: CanonicalUuid::parse(attempt).expect("id"),
    };
    let receipt = WireReceipt::sign(binding, ReceiptDisposition::Stored, facility).expect("sign");
    json!({"seq": seq, "receipt": receipt, "recorded_at": "2026-09-23T20:00:01Z"})
}

#[test]
fn a_malformed_receipt_does_not_hide_the_page() {
    let facility = InMemoryKeyFacility::generate().expect("facility");
    let mut bad_id = signed_receipt_item(&facility, 40, ATTEMPT);
    bad_id["receipt"]["binding"]["delivery_attempt_id"] = json!("not-a-uuid");
    let mut short_sig = signed_receipt_item(&facility, 41, "01920000-0000-7000-8000-0000000e0002");
    let sig = short_sig["receipt"]["signature"]
        .as_str()
        .expect("signature")
        .to_owned();
    // 63 bytes: drop the last base64url quantum's final byte by re-encoding a truncated value
    let raw = khive_channel_node::encoding::decode_base64url(&sig).expect("decode");
    short_sig["receipt"]["signature"] =
        json!(khive_channel_node::encoding::encode_base64url(&raw[..63]));
    let good = signed_receipt_item(&facility, 42, "01920000-0000-7000-8000-0000000e0003");

    let page = json!({
        "deliveries": [],
        "receipts": [bad_id, short_sig, good],
        "receipts_cursor": 42,
        "server_time": "2026-09-23T20:00:02Z",
    });
    let parsed: PollResponse = serde_json::from_value(page)
        .expect("one malformed receipt must not make the page unreadable");
    let written = serde_json::to_value(&parsed).expect("serialize");
    assert_eq!(
        written["receipts_cursor"], 42,
        "the cursor must stay readable"
    );
}

// ---- degenerate signing keys are refused (A.3) ----

fn degenerate_keys() -> [(&'static str, [u8; 32]); 3] {
    let mut identity = [0u8; 32];
    identity[0] = 1;
    let mut identity_sign_bit = identity;
    identity_sign_bit[31] = 0x80;
    let mut y_is_p_plus_one = [0xffu8; 32];
    y_is_p_plus_one[0] = 0xee;
    y_is_p_plus_one[31] = 0x7f;
    [
        ("identity point", identity),
        ("identity point with the sign bit set", identity_sign_bit),
        ("y = p + 1 (non-canonical)", y_is_p_plus_one),
    ]
}

/// R = identity, S = 0: verifies for every message under the identity key.
fn forged_signature() -> [u8; 64] {
    let mut s = [0u8; 64];
    s[0] = 1;
    s
}

#[test]
fn degenerate_signing_keys_are_refused_and_never_verify() {
    let facility = InMemoryKeyFacility::generate().expect("facility");
    let kem_hex = serde_json::to_value(facility.public_keys().kem).expect("kem");
    for (name, bytes) in degenerate_keys() {
        // The key type itself must refuse it, through construction and through JSON.
        let built = SigningPublicKey::new(bytes);
        let parsed: Result<SigningPublicKey, _> = serde_json::from_value(json!(hex::encode(bytes)));
        // And if a value of the type exists at all, no forged signature verifies under it.
        if let Ok(key) = &built {
            assert!(
                key.verify(b"message A", &forged_signature()).is_err(),
                "{name}: forged signature verified"
            );
            let headers = RequestHeaders {
                device: CanonicalUuid::parse(ATTEMPT).expect("id"),
                timestamp: 1_790_000_000,
                nonce: khive_channel_node::encoding::HexBytes::new([7; 16]),
                signature: khive_channel_node::encoding::Base64Bytes::new(forged_signature()),
            };
            assert!(
                headers
                    .verify(key, "POST", "/node/v1/receipts", b"{}")
                    .is_err(),
                "{name}: forged request verified"
            );
        }
        let bundle: Result<EnrolmentBundle, _> = serde_json::from_value(json!({
            "realm": "khive.test",
            "kem_public_key": kem_hex,
            "signing_public_key": hex::encode(bytes),
            "enrol_proof": khive_channel_node::encoding::encode_base64url(&forged_signature()),
        }));
        if let Ok(bundle) = bundle {
            assert!(
                bundle.verify().is_err(),
                "{name}: forged enrolment bundle verified"
            );
        }
        assert!(built.is_err(), "{name}: SigningPublicKey::new accepted it");
        assert!(parsed.is_err(), "{name}: SigningPublicKey deserialized it");
    }
    // control: a real key still verifies its own signature and refuses the forged one
    let real = facility.public_keys().signing;
    let sig = facility.sign(b"message A");
    assert!(real.verify(b"message A", &sig).is_ok());
    assert!(real.verify(b"message A", &forged_signature()).is_err());
}

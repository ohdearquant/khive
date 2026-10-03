use super::*;
use crate::encoding::{decode_hex, Epoch, ProtocolVersion};
use crate::receipt::{WireReceipt, WireReceiptBinding};
use crate::request::RequestHeaders;
use crate::wire::ContactResponse;
use khive_channel::ReceiptDisposition;
use serde_json::{json, Value};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../tests/fixtures/node-v1-vectors.json")).unwrap()
}

fn value<'a>(fixture: &'a Value, group: &str, key: &str) -> &'a str {
    fixture["values"][group]
        .as_array()
        .unwrap()
        .iter()
        .find(|pair| pair["key"] == key)
        .unwrap()["value"]
        .as_str()
        .unwrap()
}

fn fixed<const N: usize>(fixture: &Value, group: &str, key: &str) -> [u8; N] {
    decode_hex(value(fixture, group, key))
        .unwrap()
        .try_into()
        .unwrap()
}

fn identity() -> [u8; 32] {
    let mut bytes = [0; 32];
    bytes[0] = 1;
    bytes
}

fn forged_signature() -> [u8; 64] {
    let mut bytes = [0; 64];
    bytes[0] = 1;
    bytes
}

fn assert_key_refused(bytes: [u8; 32]) {
    assert_eq!(SigningPublicKey::new(bytes), Err(ProtocolError::InvalidKey));
    assert!(serde_json::from_value::<SigningPublicKey>(json!(hex::encode(bytes))).is_err());
}

#[test]
fn signing_key_refuses_canonical_identity_point() {
    let bytes = identity();
    let point = CompressedEdwardsY(bytes).decompress().unwrap();
    assert!(point.is_small_order());
    assert_eq!(point.compress().to_bytes(), bytes);
    assert_key_refused(bytes);
}

#[test]
fn signing_key_refuses_identity_with_sign_bit() {
    let mut bytes = identity();
    bytes[31] = 0x80;
    assert_key_refused(bytes);
}

#[test]
fn signing_key_refuses_y_equal_to_p_plus_one() {
    let mut bytes = [0xff; 32];
    bytes[0] = 0xee;
    bytes[31] = 0x7f;
    assert_key_refused(bytes);
}

#[test]
fn signing_key_refuses_noncanonical_points_without_small_order() {
    let mut witnesses = 0;
    // These are the nineteen possible noncanonical y values below 2^255.
    for low in 0xed..=0xff {
        let mut bytes = [0xff; 32];
        bytes[0] = low;
        bytes[31] = 0x7f;
        if let Some(point) = CompressedEdwardsY(bytes)
            .decompress()
            .filter(|point| !point.is_small_order())
        {
            assert_ne!(point.compress().to_bytes(), bytes);
            witnesses += 1;
            assert_key_refused(bytes);
        }
    }
    assert!(
        witnesses > 0,
        "a non-small-order round-trip witness must exist"
    );
}

#[test]
fn enrolment_refuses_identity_forgery_at_key_admission() {
    let fixture = fixture();
    let bundle = serde_json::from_value::<EnrolmentBundle>(json!({
        "realm": "relay.example",
        "kem_public_key": value(&fixture, "device_keys_recipient", "kem_public_key"),
        "signing_public_key": hex::encode(identity()),
        "enrol_proof": crate::encoding::encode_base64url(&forged_signature()),
    }));
    if let Ok(bundle) = &bundle {
        assert_eq!(bundle.verify(), Err(ProtocolError::InvalidProof));
    }
    assert!(
        bundle.is_err(),
        "enrolment must refuse the key while decoding"
    );
}

#[test]
fn request_refuses_identity_forgery_at_key_admission() {
    let key = serde_json::from_value::<SigningPublicKey>(json!(hex::encode(identity())));
    if let Ok(key) = &key {
        let headers = RequestHeaders {
            device: CanonicalUuid::parse("01920000-0000-7000-8000-00000000d001").unwrap(),
            timestamp: 1_790_193_600,
            nonce: HexBytes::new([7; 16]),
            signature: Base64Bytes::new(forged_signature()),
        };
        assert_eq!(
            headers.verify(key, "POST", "/node/v1/receipts", b"{}"),
            Err(ProtocolError::InvalidSignature),
        );
    }
    assert!(key.is_err(), "request pin decoding must refuse the key");
}

fn receipt_binding(fixture: &Value) -> WireReceiptBinding {
    let group = "receipt_binding";
    let id = |key| CanonicalUuid::parse(value(fixture, group, key)).unwrap();
    WireReceiptBinding {
        protocol_version: ProtocolVersion::new(1).unwrap(),
        logical_message_id: id("logical_message_id"),
        sender_agent_id: id("sender_agent_id"),
        recipient_agent_id: id("recipient_agent_id"),
        recipient_device_id: id("recipient_device_id"),
        recipient_key_epoch: Epoch::new(
            value(fixture, group, "recipient_key_epoch")
                .parse()
                .unwrap(),
        )
        .unwrap(),
        contact_generation: Epoch::new(
            value(fixture, group, "contact_generation").parse().unwrap(),
        )
        .unwrap(),
        delivery_attempt_id: id("delivery_attempt_id"),
    }
}

#[test]
fn receipt_refuses_identity_forgery_at_key_admission() {
    let fixture = fixture();
    let key = SigningPublicKey::new(identity());
    if let Ok(key) = &key {
        let receipt = WireReceipt {
            binding: receipt_binding(&fixture),
            disposition: ReceiptDisposition::Stored,
            signature: Base64Bytes::new(forged_signature()),
        };
        assert_eq!(receipt.verify(key), Err(ProtocolError::InvalidSignature));
    }
    assert_eq!(key, Err(ProtocolError::InvalidKey));
}

#[test]
fn contact_pin_refuses_degenerate_signing_key_during_decode() {
    let fixture = fixture();
    let agent = value(&fixture, "envelope", "recipient_agent_id");
    let contact = json!({
        "agent_id": agent,
        "address": format!("khive1:relay.example/{agent}"),
        "device_id": value(&fixture, "envelope", "recipient_device_id"),
        "key_epoch": 2,
        "kem_public_key": value(&fixture, "device_keys_recipient", "kem_public_key"),
        "signing_public_key": hex::encode(identity()),
        "fingerprint": value(&fixture, "device_keys_recipient", "fingerprint"),
        "contact_generation": 3,
    });
    assert!(serde_json::from_value::<ContactResponse>(contact).is_err());
}

#[test]
fn canonical_recipient_key_keeps_a11_proof_and_receipt_vectors() {
    let fixture = fixture();
    let key = SigningPublicKey::new(fixed(
        &fixture,
        "device_keys_recipient",
        "signing_public_key",
    ))
    .unwrap();
    let proof_input = decode_hex(value(
        &fixture,
        "device_keys_recipient",
        "enrol_proof_input",
    ))
    .unwrap();
    key.verify(
        &proof_input,
        &fixed(&fixture, "device_keys_recipient", "enrol_proof"),
    )
    .unwrap();
    let bundle = EnrolmentBundle {
        realm: Realm::parse("relay.example").unwrap(),
        kem_public_key: KemPublicKey::new(fixed(
            &fixture,
            "device_keys_recipient",
            "kem_public_key",
        ))
        .unwrap(),
        signing_public_key: key.clone(),
        enrol_proof: Base64Bytes::new(fixed(&fixture, "device_keys_recipient", "enrol_proof")),
    };
    assert_eq!(bundle.verify().unwrap().signing, key);
    for (group, disposition) in [
        ("receipt_stored", ReceiptDisposition::Stored),
        ("receipt_quarantined", ReceiptDisposition::Quarantined),
    ] {
        let receipt = WireReceipt {
            binding: receipt_binding(&fixture),
            disposition,
            signature: Base64Bytes::new(fixed(&fixture, group, "signature")),
        };
        receipt.verify(&key).unwrap();
        let mut forged = receipt;
        forged.signature = Base64Bytes::new(forged_signature());
        assert_eq!(forged.verify(&key), Err(ProtocolError::InvalidSignature));
    }
}

#[test]
fn canonical_recipient_key_refuses_noncanonical_r_and_s_at_order() {
    let fixture = fixture();
    let key = SigningPublicKey::new(fixed(
        &fixture,
        "device_keys_recipient",
        "signing_public_key",
    ))
    .unwrap();
    let input = decode_hex(value(&fixture, "receipt_stored", "signing_input")).unwrap();
    let signature = fixed::<64>(&fixture, "receipt_stored", "signature");
    key.verify(&input, &signature).unwrap();
    let mut negative_zero = identity();
    negative_zero[31] = 0x80;
    let mut y_is_p_plus_one = [0xff; 32];
    y_is_p_plus_one[0] = 0xee;
    y_is_p_plus_one[31] = 0x7f;
    for encoded_r in [negative_zero, y_is_p_plus_one] {
        let mut invalid = signature;
        invalid[..32].copy_from_slice(&encoded_r);
        assert_eq!(
            key.verify(&input, &invalid),
            Err(ProtocolError::InvalidSignature)
        );
    }
    let mut invalid = signature;
    let order =
        decode_hex("edd3f55c1a631258d69cf7a2def9de1400000000000000000000000000000010").unwrap();
    invalid[32..].copy_from_slice(&order);
    assert_eq!(
        key.verify(&input, &invalid),
        Err(ProtocolError::InvalidSignature)
    );
}

fn envelope_header(fixture: &Value) -> EnvelopeHeader {
    let id = |key| CanonicalUuid::parse(value(fixture, "envelope", key)).unwrap();
    EnvelopeHeader {
        protocol_version: ProtocolVersion::new(1).unwrap(),
        realm: Realm::parse("relay.example").unwrap(),
        sender_agent_id: id("sender_agent_id"),
        sender_device_id: id("sender_device_id"),
        sender_key_epoch: Epoch::new(1).unwrap(),
        recipient_agent_id: id("recipient_agent_id"),
        recipient_device_id: id("recipient_device_id"),
        recipient_key_epoch: Epoch::new(2).unwrap(),
    }
}

struct FailingEntropy {
    infallible_calls: usize,
    fallible_calls: usize,
}
impl rand_core::CryptoRng for FailingEntropy {}
impl RngCore for FailingEntropy {
    fn next_u32(&mut self) -> u32 {
        panic!("unexpected integer entropy request")
    }
    fn next_u64(&mut self) -> u64 {
        panic!("unexpected integer entropy request")
    }
    fn fill_bytes(&mut self, _dest: &mut [u8]) {
        self.infallible_calls += 1;
        panic!("infallible entropy fill must not be called")
    }
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.fallible_calls += 1;
        dest.fill(0xa5);
        Err(std::num::NonZeroU32::new(rand_core::Error::CUSTOM_START)
            .unwrap()
            .into())
    }
}

#[test]
fn seal_reports_randomness_failure_without_panicking() {
    let fixture = fixture();
    let sender = InMemoryKeyFacility::from_test_seeds(
        &fixed(&fixture, "device_keys_sender", "kem_ikm"),
        &fixed(&fixture, "device_keys_sender", "signing_seed"),
    );
    let recipient =
        KemPublicKey::new(fixed(&fixture, "device_keys_recipient", "kem_public_key")).unwrap();
    let mut rng = FailingEntropy {
        infallible_calls: 0,
        fallible_calls: 0,
    };
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        sender.seal_with_rng(
            &envelope_header(&fixture),
            CanonicalUuid::parse(value(&fixture, "envelope", "logical_message_id")).unwrap(),
            &recipient,
            b"test-only plaintext",
            &mut rng,
        )
    }));
    assert!(
        outcome.is_ok(),
        "entropy failure must return a typed refusal"
    );
    assert_eq!(outcome.unwrap(), Err(ProtocolError::Randomness));
    assert_eq!(rng.infallible_calls, 0);
    assert_eq!(rng.fallible_calls, 1);
}

#[test]
fn successful_entropy_preserves_a11_encapsulation_bytes() {
    let fixture = fixture();
    let sender = InMemoryKeyFacility::from_test_seeds(
        &fixed(&fixture, "device_keys_sender", "kem_ikm"),
        &fixed(&fixture, "device_keys_sender", "signing_seed"),
    );
    let recipient =
        KemPublicKey::new(fixed(&fixture, "device_keys_recipient", "kem_public_key")).unwrap();
    let plaintext = decode_hex(value(&fixture, "envelope", "plaintext")).unwrap();
    let actual = sender
        .seal_with_rng(
            &envelope_header(&fixture),
            CanonicalUuid::parse(value(&fixture, "envelope", "logical_message_id")).unwrap(),
            &recipient,
            &plaintext,
            &mut TestEphemeral(fixed(&fixture, "envelope", "ephemeral_ikm")),
        )
        .unwrap();
    assert_eq!(
        actual.enc.as_bytes(),
        &fixed::<32>(&fixture, "envelope", "enc")
    );
    assert_eq!(
        actual.ciphertext.as_bytes(),
        decode_hex(value(&fixture, "envelope", "ciphertext")).unwrap()
    );
}

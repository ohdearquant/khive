use crate::encoding::{decode_hex, encode_base64url, Realm};
use crate::keys::{EnrolmentBundle, SigningPublicKey};
use crate::plaintext::{classify_plaintext, InvalidPlaintextReason, PlaintextClassification};
use crate::wire::{AdmissionResponse, ContactResponse, Delivery, PollResponse, RefusalCode};
use crate::ProtocolError;
use serde_json::{json, Value};

fn fixture() -> Value {
    serde_json::from_str(include_str!("../tests/fixtures/node-v1-vectors.json")).unwrap()
}

fn value<'a>(fixture: &'a Value, group: &str, key: &str) -> &'a str {
    fixture["values"][group]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["key"] == key)
        .unwrap()["value"]
        .as_str()
        .unwrap()
}

fn input<'a>(fixture: &'a Value, name: &str, required_outcome: &str) -> &'a Value {
    let input = &fixture["supplemental_inputs"][name];
    let row = fixture["conformance_cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["case"] == input["case"])
        .expect("executable input must name its A.11 row");
    assert_eq!(row["required_outcome"], required_outcome);
    input
}

fn bytes(fixture: &Value, group: &str, key: &str) -> Vec<u8> {
    decode_hex(value(fixture, group, key)).unwrap()
}

fn contact(fixture: &Value) -> Value {
    json!({
        "agent_id": value(fixture, "envelope", "recipient_agent_id"),
        "address": format!("khive1:{}/{}",
            value(fixture, "envelope_context", "realm"),
            value(fixture, "envelope", "recipient_agent_id")),
        "device_id": value(fixture, "envelope", "recipient_device_id"),
        "key_epoch": 2,
        "kem_public_key": value(fixture, "device_keys_recipient", "kem_public_key"),
        "signing_public_key": value(fixture, "device_keys_recipient", "signing_public_key"),
        "fingerprint": value(fixture, "device_keys_recipient", "fingerprint"),
        "contact_generation": 3,
    })
}

fn enrolment(fixture: &Value) -> Value {
    json!({
        "realm": value(fixture, "envelope_context", "realm"),
        "kem_public_key": value(fixture, "device_keys_recipient", "kem_public_key"),
        "signing_public_key": value(fixture, "device_keys_recipient", "signing_public_key"),
        "enrol_proof": encode_base64url(&bytes(fixture, "device_keys_recipient", "enrol_proof")),
    })
}

fn delivery(fixture: &Value) -> Value {
    json!({
        "delivery_attempt_id": value(fixture, "receipt_binding", "delivery_attempt_id"),
        "logical_message_id": value(fixture, "envelope", "logical_message_id"),
        "protocol_version": 1,
        "sender_agent_id": value(fixture, "envelope", "sender_agent_id"),
        "sender_device_id": value(fixture, "envelope", "sender_device_id"),
        "sender_key_epoch": 1,
        "recipient_agent_id": value(fixture, "envelope", "recipient_agent_id"),
        "recipient_device_id": value(fixture, "envelope", "recipient_device_id"),
        "recipient_key_epoch": 2,
        "contact_generation": 3,
        "enc": encode_base64url(&bytes(fixture, "envelope", "enc")),
        "ciphertext": encode_base64url(&bytes(fixture, "envelope", "ciphertext")),
    })
}

fn receipt_item(fixture: &Value) -> Value {
    json!({
        "seq": 41,
        "recorded_at": "2026-09-23T20:00:00Z",
        "receipt": {
            "binding": {
                "protocol_version": 1,
                "logical_message_id": value(fixture, "receipt_binding", "logical_message_id"),
                "sender_agent_id": value(fixture, "receipt_binding", "sender_agent_id"),
                "recipient_agent_id": value(fixture, "receipt_binding", "recipient_agent_id"),
                "recipient_device_id": value(fixture, "receipt_binding", "recipient_device_id"),
                "recipient_key_epoch": 2,
                "contact_generation": 3,
                "delivery_attempt_id": value(fixture, "receipt_binding", "delivery_attempt_id"),
            },
            "disposition": "stored",
            "signature": encode_base64url(&bytes(fixture, "receipt_stored", "signature")),
        },
    })
}

fn assert_invalid_sent_at(name: &str) {
    let fixture = fixture();
    let row = input(&fixture, name, "`quarantined` receipt, no message note");
    assert!(matches!(
        classify_plaintext(value(&fixture, "plaintext_json", "utf8").as_bytes()),
        PlaintextClassification::Valid(_)
    ));
    assert_eq!(
        classify_plaintext(row["plaintext"].as_str().unwrap().as_bytes()),
        PlaintextClassification::Invalid(InvalidPlaintextReason::NotObject)
    );
}

#[test]
fn a11_sent_at_numeric_offset_is_invalid() {
    assert_invalid_sent_at("sent_at_offset");
}

#[test]
fn a11_sent_at_space_separator_is_invalid() {
    assert_invalid_sent_at("sent_at_space");
}

#[test]
fn a11_sent_at_unknown_offset_is_invalid() {
    assert_invalid_sent_at("sent_at_unknown_offset");
}

#[test]
fn a11_server_numeric_offset_normalizes_to_utc() {
    let fixture = fixture();
    let row = input(
        &fixture,
        "server_timestamp_offset",
        "accepted and normalized to `2026-09-23T20:00:00Z`",
    );
    let admission: AdmissionResponse = serde_json::from_value(json!({
        "state": "pending",
        "delivery_attempt_id": value(&fixture, "receipt_binding", "delivery_attempt_id"),
        "admitted_at": row["timestamp"],
    }))
    .expect("the A.11 server offset must parse");
    assert_eq!(
        serde_json::to_value(admission).unwrap()["admitted_at"],
        row["normalized"]
    );
}

fn assert_bad_signing_key(name: &str, pin: bool) {
    let fixture = fixture();
    let row = input(&fixture, name, "refused at enrolment and at pin");
    let realm = Realm::parse(value(&fixture, "envelope_context", "realm")).unwrap();
    let original_contact: ContactResponse = serde_json::from_value(contact(&fixture)).unwrap();
    let keys = original_contact.validated_keys(&realm).unwrap();
    let original_enrolment: EnrolmentBundle = serde_json::from_value(enrolment(&fixture)).unwrap();
    assert_eq!(original_enrolment.verify().unwrap(), keys);
    for key in row["signing_keys"].as_array().unwrap() {
        let mut candidate = if pin {
            contact(&fixture)
        } else {
            enrolment(&fixture)
        };
        candidate["signing_public_key"] = key.clone();
        if pin {
            assert!(
                serde_json::from_value::<ContactResponse>(candidate).is_err(),
                "a degenerate signing key cannot reach pin confirmation: {key}"
            );
        } else {
            assert!(
                serde_json::from_value::<EnrolmentBundle>(candidate).is_err(),
                "a degenerate signing key cannot reach enrolment verification: {key}"
            );
        }
        let bytes: [u8; 32] = decode_hex(key.as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(SigningPublicKey::new(bytes), Err(ProtocolError::InvalidKey));
    }
}

#[test]
fn a11_identity_signing_key_is_refused_at_enrolment() {
    assert_bad_signing_key("signing_key_identity", false);
}

#[test]
fn a11_identity_signing_key_is_refused_at_pin() {
    assert_bad_signing_key("signing_key_identity", true);
}

#[test]
fn a11_noncanonical_signing_keys_are_refused_at_enrolment() {
    assert_bad_signing_key("signing_key_noncanonical", false);
}

#[test]
fn a11_noncanonical_signing_keys_are_refused_at_pin() {
    assert_bad_signing_key("signing_key_noncanonical", true);
}

#[test]
fn a11_nested_duplicate_member_is_invalid() {
    let fixture = fixture();
    let row = input(
        &fixture,
        "nested_duplicate",
        "`quarantined` receipt, no message note",
    );
    let original = value(&fixture, "plaintext_json", "utf8");
    let unique = format!(
        "{},\"extension\":{{\"x\":1}}}}",
        original.trim_end_matches('}')
    );
    assert!(matches!(
        classify_plaintext(unique.as_bytes()),
        PlaintextClassification::Valid(_)
    ));
    assert_eq!(
        classify_plaintext(row["plaintext"].as_str().unwrap().as_bytes()),
        PlaintextClassification::Invalid(InvalidPlaintextReason::DuplicateMember)
    );
}

#[test]
fn a11_malformed_delivery_keeps_valid_items_and_cursor() {
    let fixture = fixture();
    let row = input(
        &fixture,
        "malformed_delivery",
        "malformed delivery reported and skipped with no receipt; valid items and cursor remain readable",
    );
    let valid = delivery(&fixture);
    let parsed: Delivery = serde_json::from_value(valid.clone()).unwrap();
    assert_eq!(
        parsed.delivery_attempt_id.to_string(),
        value(&fixture, "receipt_binding", "delivery_attempt_id")
    );
    let mut malformed = valid.clone();
    malformed["delivery_attempt_id"] = row["delivery_attempt_id"].clone();
    let page: PollResponse = serde_json::from_value(json!({
        "deliveries": [malformed, valid],
        "receipts": [receipt_item(&fixture)],
        "receipts_cursor": row["receipts_cursor"],
        "server_time": "2026-09-23T20:00:00Z",
    }))
    .expect("one malformed delivery must not invalidate its poll page");
    assert_eq!(page.deliveries.as_slice().len(), 2);
    let rejected = &page.deliveries.as_slice()[0];
    assert_eq!(rejected.index, 0);
    assert_eq!(
        rejected.result.as_ref().unwrap_err().code,
        RefusalCode::InvalidRequest
    );
    let accepted = &page.deliveries.as_slice()[1];
    assert_eq!(accepted.index, 1);
    assert_eq!(
        accepted.result.as_ref().unwrap().delivery_attempt_id,
        parsed.delivery_attempt_id
    );
    assert_eq!(
        page.deliveries
            .as_slice()
            .iter()
            .filter(|item| item.result.is_ok())
            .count(),
        1,
        "only the valid delivery can supply a typed receipt binding"
    );
    assert_eq!(
        page.receipts_cursor.get(),
        row["receipts_cursor"].as_u64().unwrap()
    );
    assert_eq!(page.receipts.as_slice().len(), 1);
    let receipt = page.receipts.as_slice()[0].result.as_ref().unwrap();
    assert_eq!(receipt.seq.get(), 41);
    let keys = serde_json::from_value::<ContactResponse>(contact(&fixture))
        .unwrap()
        .validated_keys(&Realm::parse(value(&fixture, "envelope_context", "realm")).unwrap())
        .unwrap();
    receipt.receipt.verify(&keys.signing).unwrap();
}

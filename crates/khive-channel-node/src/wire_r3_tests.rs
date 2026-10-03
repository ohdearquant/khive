use crate::encoding::{CanonicalUuid, Epoch, JsonInteger, ProtocolVersion};
use crate::keys::{InMemoryKeyFacility, KeyFacility};
use crate::receipt::{WireReceipt, WireReceiptBinding};
use crate::wire::{Delivery, PollResponse, RefusalCode, SubmitRequest, WireDecodeError};
use crate::ProtocolError;
use khive_channel::ReceiptDisposition;
use serde_json::{json, Value};

fn receipt(facility: &InMemoryKeyFacility, seq: u64) -> Value {
    let id = CanonicalUuid::parse("01920000-0000-7000-8000-00000000a001").unwrap();
    let binding = WireReceiptBinding {
        protocol_version: ProtocolVersion::new(1).unwrap(),
        logical_message_id: id,
        sender_agent_id: id,
        recipient_agent_id: id,
        recipient_device_id: id,
        recipient_key_epoch: Epoch::new(1).unwrap(),
        contact_generation: Epoch::new(1).unwrap(),
        delivery_attempt_id: id,
    };
    json!({"seq":seq,"receipt":WireReceipt::sign(binding,ReceiptDisposition::Stored,facility).unwrap(),
        "recorded_at":"2026-09-23T20:00:01Z"})
}
fn submit() -> Value {
    json!({"protocol_version":1,"logical_message_id":"01920000-0000-7000-8000-00000000a001",
        "recipient":"khive1:khive.test/01920000-0000-7000-8000-00000000a001",
        "recipient_device_id":"01920000-0000-7000-8000-00000000a001",
        "recipient_key_epoch":1,"sender_key_epoch":1,"contact_generation":1,
        "enc":crate::encoding::encode_base64url(&[5;32]),"ciphertext":""})
}
fn delivery() -> Value {
    let mut value = submit();
    let object = value.as_object_mut().unwrap();
    object.remove("recipient");
    for field in [
        "delivery_attempt_id",
        "sender_agent_id",
        "sender_device_id",
        "recipient_agent_id",
    ] {
        object.insert(
            field.to_owned(),
            json!("01920000-0000-7000-8000-00000000a001"),
        );
    }
    value
}
#[test]
fn malformed_poll_receipts_keep_indices_valid_later_signature_and_cursor() {
    let facility = InMemoryKeyFacility::generate().unwrap();
    let mut bad_id = receipt(&facility, 40);
    bad_id["receipt"]["binding"]["delivery_attempt_id"] = json!("not-a-uuid");
    let mut short = receipt(&facility, 41);
    short["receipt"]["signature"] = json!(crate::encoding::encode_base64url(&[0; 63]));
    let page: PollResponse = serde_json::from_value(json!({"deliveries":[],
        "receipts":[bad_id,short,receipt(&facility,42)],"receipts_cursor":42,
        "server_time":"2026-09-23T16:00:02-04:00"}))
    .unwrap();
    assert_eq!(page.receipts_cursor, JsonInteger::new(42).unwrap());
    assert_eq!(page.receipts.as_slice().len(), 3);
    for (index, item) in page.receipts.as_slice().iter().take(2).enumerate() {
        assert_eq!(item.index, index);
        assert_eq!(
            item.result.as_ref().unwrap_err().code,
            RefusalCode::InvalidRequest
        );
    }
    let last = &page.receipts.as_slice()[2];
    assert_eq!(last.index, 2);
    last.result
        .as_ref()
        .unwrap()
        .receipt
        .verify(&facility.public_keys().signing)
        .unwrap();
}
#[test]
fn malformed_delivery_does_not_hide_receipts_or_cursor() {
    let facility = InMemoryKeyFacility::generate().unwrap();
    let page: PollResponse =
        serde_json::from_value(json!({"deliveries":[{"unexpected":1},delivery()],
        "receipts":[receipt(&facility,42)],"receipts_cursor":42,
        "server_time":"2026-09-23T20:00:02Z"}))
        .unwrap();
    assert_eq!(page.deliveries.as_slice()[0].index, 0);
    assert!(page.deliveries.as_slice()[0].result.is_err());
    assert!(page.deliveries.as_slice()[1].result.is_ok());
    assert_eq!(page.receipts_cursor.get(), 42);
    page.receipts.as_slice()[0]
        .result
        .as_ref()
        .unwrap()
        .receipt
        .verify(&facility.public_keys().signing)
        .unwrap();
}
#[test]
fn poll_page_own_members_remain_strict_and_bounded() {
    let valid = json!({"deliveries":[],"receipts":[],"receipts_cursor":0,
        "server_time":"2026-09-23T20:00:02Z"});
    serde_json::from_value::<PollResponse>(valid.clone()).unwrap();
    for field in ["deliveries", "receipts", "receipts_cursor", "server_time"] {
        let mut missing = valid.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(
            serde_json::from_value::<PollResponse>(missing).is_err(),
            "{field}"
        );
    }
    let mut extra = valid.clone();
    extra["unknown"] = json!(1);
    assert!(serde_json::from_value::<PollResponse>(extra).is_err());
    for (field, invalid) in [
        ("deliveries", json!({})),
        ("receipts", json!(null)),
        ("receipts_cursor", json!("42")),
        ("server_time", json!("yesterday")),
        ("deliveries", json!(vec![json!({}); 17])),
        ("receipts", json!(vec![json!({}); 65])),
    ] {
        let mut page = valid.clone();
        page[field] = invalid;
        assert!(
            serde_json::from_value::<PollResponse>(page).is_err(),
            "{field}"
        );
    }
    assert!(serde_json::from_str::<PollResponse>(
        r#"{"deliveries":[],"receipts":[],"receipts_cursor":0,"receipts_cursor":1,"server_time":"2026-09-23T20:00:02Z"}"#).is_err());
}
#[test]
fn duplicated_delivery_member_is_reported_per_item() {
    let body = format!(
        r#"{{"deliveries":[{}],"receipts":[],"receipts_cursor":0,"server_time":"2026-09-23T20:00:02Z"}}"#,
        serde_json::to_string(&delivery()).unwrap().replacen(
            "\"protocol_version\":1",
            "\"protocol_version\":1,\"protocol_version\":1",
            1
        )
    );
    let page: PollResponse = serde_json::from_str(&body).unwrap();
    assert!(page.deliveries.as_slice()[0].result.is_err());
    assert_eq!(page.deliveries.as_slice()[0].index, 0);
}
#[test]
fn typed_wire_decode_preserves_version_and_size_refusal_classes() {
    for (name, mut value) in [("submit", submit()), ("delivery", delivery())] {
        value["protocol_version"] = json!(2);
        let bytes = serde_json::to_vec(&value).unwrap();
        let error = if name == "submit" {
            SubmitRequest::parse(&bytes).unwrap_err()
        } else {
            Delivery::parse(&bytes).unwrap_err()
        };
        assert!(matches!(
            error,
            WireDecodeError::Protocol(ProtocolError::UnsupportedVersion)
        ));
        assert_eq!(error.refusal_code(), RefusalCode::UnsupportedVersion);
        value["protocol_version"] = json!(1);
        value["ciphertext"] = json!(crate::encoding::encode_base64url(&vec![0; 65_537]));
        let bytes = serde_json::to_vec(&value).unwrap();
        let error = if name == "submit" {
            SubmitRequest::parse(&bytes).unwrap_err()
        } else {
            Delivery::parse(&bytes).unwrap_err()
        };
        assert!(matches!(
            error,
            WireDecodeError::Protocol(ProtocolError::EnvelopeTooLarge)
        ));
        assert_eq!(error.refusal_code(), RefusalCode::PayloadTooLarge);
        value["ciphertext"] = json!("");
        value["unknown"] = json!(1);
        let bytes = serde_json::to_vec(&value).unwrap();
        let error = if name == "submit" {
            SubmitRequest::parse(&bytes).unwrap_err()
        } else {
            Delivery::parse(&bytes).unwrap_err()
        };
        assert_eq!(error.refusal_code(), RefusalCode::InvalidRequest);
    }
}

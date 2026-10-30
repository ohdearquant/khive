//! JSON protocol guard fixtures use only the original public API.
use khive_channel::ReceiptDisposition;
use khive_channel_node::encoding::{CanonicalUuid, Epoch, ProtocolVersion};
use khive_channel_node::keys::{InMemoryKeyFacility, KeyFacility};
use khive_channel_node::receipt::{WireReceipt, WireReceiptBinding};
use khive_channel_node::wire::{
    AdmissionResponse, MessageState, PendingState, PollResponse, RefusalCode, UtcTimestamp,
};
use serde_json::{json, Value};

fn receipt_item(facility: &InMemoryKeyFacility, seq: u64) -> Value {
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
        "recorded_at":"2026-09-23T20:00:00Z"})
}
fn page(deliveries: Value, receipts: Value) -> Value {
    json!({"deliveries":deliveries,"receipts":receipts,"receipts_cursor":42,
        "server_time":"2026-09-23T20:00:00Z"})
}
#[test]
fn unit_enums_refuse_object_forms() {
    assert!(serde_json::from_value::<PendingState>(json!("pending")).is_ok());
    assert!(serde_json::from_value::<MessageState>(json!("recipient_stored")).is_ok());
    assert!(serde_json::from_value::<RefusalCode>(json!("rate_limited")).is_ok());
    assert!(serde_json::from_value::<ReceiptDisposition>(json!("stored")).is_ok());
    assert!(serde_json::from_value::<PendingState>(json!({"pending":null})).is_err());
    assert!(serde_json::from_value::<MessageState>(json!({"recipient_stored":null})).is_err());
    assert!(serde_json::from_value::<RefusalCode>(json!({"rate_limited":null})).is_err());
    assert!(serde_json::from_value::<ReceiptDisposition>(json!({"stored":null})).is_err());
    assert_eq!(
        serde_json::from_value::<RefusalCode>(json!("unrecognized")).unwrap(),
        RefusalCode::InvalidRequest
    );
}
#[test]
fn utc_timestamp_profile_refuses_lenient_forms() {
    for good in [
        "2026-09-23T20:00:00Z",
        "2026-09-23T20:00:00+00:00",
        "2026-09-23T20:00:00.123456789Z",
    ] {
        assert!(UtcTimestamp::parse(good).is_ok(), "{good}");
    }
    for bad in [
        "2026-09-23 20:00:00Z",
        "2026-09-23t20:00:00Z",
        "2026-09-23T20:00:00z",
        "2026-09-23T20:00:00-00:00",
        "2026-09-23T23:59:60Z",
        "2026-09-23T20:00:00.1234567890Z",
    ] {
        assert!(UtcTimestamp::parse(bad).is_err(), "{bad}");
    }
}
#[test]
fn server_timestamp_profile_refuses_lenient_forms() {
    for good in ["2026-09-23T20:00:00Z", "2026-09-23T20:00:00+00:00"] {
        let body = json!({"state":"pending","delivery_attempt_id":"01920000-0000-7000-8000-0000000e0001","admitted_at":good});
        assert!(
            serde_json::from_value::<AdmissionResponse>(body).is_ok(),
            "{good}"
        );
    }
    for bad in [
        "2026-09-23 20:00:00Z",
        "2026-09-23t20:00:00Z",
        "2026-09-23T20:00:00z",
        "2026-09-23T20:00:00-00:00",
        "2026-09-23T23:59:60Z",
        "2026-09-23T20:00:00.1234567890Z",
    ] {
        let body = json!({"state":"pending","delivery_attempt_id":"01920000-0000-7000-8000-0000000e0001","admitted_at":bad});
        assert!(
            serde_json::from_value::<AdmissionResponse>(body).is_err(),
            "{bad}"
        );
    }
}
#[test]
fn malformed_delivery_is_skipped_while_a_signed_receipt_stays_readable() {
    let facility = InMemoryKeyFacility::generate().unwrap();
    let parsed: PollResponse = serde_json::from_value(page(
        json!([{"bad":"item"}]),
        json!([receipt_item(&facility, 42)]),
    ))
    .expect("one malformed delivery cannot discard the poll page");
    let written = serde_json::to_value(parsed).unwrap();
    assert_eq!(written["receipts_cursor"], 42);
    let receipt: WireReceipt =
        serde_json::from_value(written["receipts"][0]["receipt"].clone()).unwrap();
    receipt.verify(&facility.public_keys().signing).unwrap();
}
#[test]
fn duplicate_page_member_refusal_keeps_the_original_contract() {
    assert!(serde_json::from_value::<PollResponse>(page(json!([]), json!([]))).is_ok());
    assert!(serde_json::from_str::<PollResponse>(r#"{"deliveries":[],"receipts":[],"receipts_cursor":1,"receipts_cursor":42,"server_time":"2026-09-23T20:00:00Z"}"#).is_err());
}

//! A recipient receipt may name an earlier key epoch than the current envelope after a confirmed
//! key change. Submit, poll and status all verify it against the exact historical row
//! (`OutboundSource::for_receipt`), never against the current envelope.
#![allow(unused_imports)]
use crate::client::NodeClient;
use crate::client_test_server::{Reply, ScriptedServer};
use crate::encoding::*;
use crate::keys::*;
use crate::pins::*;
use crate::receipt::*;
use crate::response::*;
use crate::source::*;
use crate::wire::*;
use async_trait::async_trait;
use khive_channel::{PendingDetail, ReceiptDisposition, SendOutcome};
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

fn fixture() -> Value {
    serde_json::from_str(include_str!("../tests/fixtures/node-v1-vectors.json")).unwrap()
}
fn value(group: &str, key: &str) -> String {
    fixture()["values"][group]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["key"] == key)
        .unwrap()["value"]
        .as_str()
        .unwrap()
        .into()
}
fn id(s: &str) -> CanonicalUuid {
    CanonicalUuid::parse(s).unwrap()
}
fn logical() -> Uuid {
    id(&value("envelope", "logical_message_id")).into_uuid()
}
fn facility(sender: bool) -> InMemoryKeyFacility {
    let group = if sender {
        "device_keys_sender"
    } else {
        "device_keys_recipient"
    };
    InMemoryKeyFacility::from_test_seeds(
        &decode_hex(&value(group, "kem_ikm"))
            .unwrap()
            .try_into()
            .unwrap(),
        &decode_hex(&value(group, "signing_seed"))
            .unwrap()
            .try_into()
            .unwrap(),
    )
}
fn binding(sender: bool) -> NodeClientBinding {
    let prefix = if sender { "sender" } else { "recipient" };
    NodeClientBinding {
        namespace: "local".into(),
        slug: "main".into(),
        realm: Realm::parse("relay.example").unwrap(),
        agent: id(&value("envelope", &format!("{prefix}_agent_id"))),
        device: id(&value("envelope", &format!("{prefix}_device_id"))),
        key_epoch: Epoch::new(if sender { 1 } else { 2 }).unwrap(),
        key_reference: "node-key".into(),
    }
}
fn identity(sender: bool) -> PinIdentity {
    let b = binding(sender);
    PinIdentity {
        realm: b.realm,
        agent: b.agent,
        device: b.device,
        epoch: b.key_epoch,
    }
}

/// The historical envelope: the recipient's device at epoch 2, which is what `receipt()` names.
fn historical() -> PersistedSubmission {
    let wire: SubmitRequest = serde_json::from_str(&value("request_body_json", "utf8")).unwrap();
    let b = binding(true);
    PersistedSubmission {
        namespace: b.namespace,
        kind: "khive".into(),
        slug: b.slug,
        key_reference: b.key_reference,
        protocol_version: wire.protocol_version,
        logical_message_id: wire.logical_message_id,
        sender_agent: b.agent,
        sender_device: b.device,
        sender_key_epoch: wire.sender_key_epoch,
        recipient_agent: wire.recipient.agent(),
        recipient: wire.recipient,
        recipient_device: wire.recipient_device_id,
        recipient_key_epoch: wire.recipient_key_epoch,
        recipient_fingerprint: facility(false).public_keys().fingerprint(),
        contact_generation: wire.contact_generation,
        enc: wire.enc,
        ciphertext: wire.ciphertext,
    }
}

/// The current envelope after a confirmed key change: the same device, epoch 3, another key.
fn current_row() -> PersistedSubmission {
    PersistedSubmission {
        recipient_key_epoch: Epoch::new(3).unwrap(),
        recipient_fingerprint: facility(true).public_keys().fingerprint(),
        ..historical()
    }
}

/// A source that keeps envelope history: `current` answers epoch 3, `for_receipt` answers the
/// exact row a receipt names, or fails when `history_error` is set.
struct HistorySource {
    history_error: bool,
}
#[async_trait]
impl OutboundSource for HistorySource {
    async fn current(&self, _logical: Uuid) -> Result<Option<PersistedSubmission>, SourceError> {
        Ok(Some(current_row()))
    }
    async fn for_receipt(
        &self,
        _logical: Uuid,
        recipient_device_id: Uuid,
        recipient_key_epoch: u64,
    ) -> Result<Option<PersistedSubmission>, SourceError> {
        if self.history_error {
            return Err(SourceError("fixture history read failure".into()));
        }
        let row = historical();
        let matches = row.recipient_device.into_uuid() == recipient_device_id
            && row.recipient_key_epoch.get() == recipient_key_epoch;
        Ok(matches.then_some(row))
    }
}

struct FixedPins(PinState);
#[async_trait]
impl PinSource for FixedPins {
    async fn resolve(&self, _identity: &PinIdentity) -> Result<PinState, PinSourceError> {
        Ok(self.0.clone())
    }
}
/// The recipient's epoch 2 pin, as `receipt()` is signed by the recipient's key.
fn recipient_pins() -> Arc<FixedPins> {
    let keys = facility(false).public_keys();
    Arc::new(FixedPins(PinState::Confirmed(
        ConfirmedPin::new(identity(false), keys.clone(), keys.fingerprint()).unwrap(),
    )))
}
fn history_client(server: &ScriptedServer, history_error: bool) -> NodeClient {
    NodeClient::test_http(
        binding(true),
        server.url(),
        Arc::new(HistorySource { history_error }),
        recipient_pins(),
        Arc::new(facility(true)),
    )
    .unwrap()
}

fn receipt() -> WireReceipt {
    let f = fixture();
    let g = "receipt_binding";
    let get = |k: &str| {
        f["values"][g]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["key"] == k)
            .unwrap()["value"]
            .as_str()
            .unwrap()
    };
    WireReceipt::sign(
        WireReceiptBinding {
            protocol_version: ProtocolVersion::new(1).unwrap(),
            logical_message_id: id(get("logical_message_id")),
            sender_agent_id: id(get("sender_agent_id")),
            recipient_agent_id: id(get("recipient_agent_id")),
            recipient_device_id: id(get("recipient_device_id")),
            recipient_key_epoch: Epoch::new(2).unwrap(),
            contact_generation: Epoch::new(3).unwrap(),
            delivery_attempt_id: id(get("delivery_attempt_id")),
        },
        ReceiptDisposition::Stored,
        &facility(false),
    )
    .unwrap()
}
/// A receipt for the same binding signed by the sender's key: wrong for the recipient's pin.
fn wrong_key_receipt() -> WireReceipt {
    WireReceipt::sign(
        receipt().binding,
        ReceiptDisposition::Stored,
        &facility(true),
    )
    .unwrap()
}
fn bad_signature_receipt() -> WireReceipt {
    let mut bad = receipt();
    bad.signature = Base64Bytes::new([0; 64]);
    bad
}
fn status_json(receipt: &WireReceipt) -> String {
    json!({"logical_message_id": logical(), "state": "recipient_stored", "delivery_attempt_id": receipt.binding.delivery_attempt_id, "receipt": receipt}).to_string()
}
fn poll_body(receipts: Vec<WireReceipt>) -> String {
    let items: Vec<Value> = receipts
        .into_iter()
        .enumerate()
        .map(|(i, r)| json!({"seq": i + 1, "receipt": r, "recorded_at": "2026-09-24T00:00:00Z"}))
        .collect();
    format!(
        "{{\"deliveries\":[],\"receipts\":{},\"receipts_cursor\":{},\"server_time\":\"2026-09-24T00:00:00Z\"}}",
        serde_json::to_string(&items).unwrap(),
        items.len()
    )
}
fn is_pending(outcome: &SendOutcome) -> bool {
    matches!(
        outcome,
        SendOutcome::Pending(PendingDetail::ReceiptUnverified { .. })
    )
}
async fn poll_verification(body: String, history_error: bool) -> ReceiptVerification {
    let server = ScriptedServer::start(vec![Reply::json(200, &body)])
        .await
        .unwrap();
    let mut page = history_client(&server, history_error)
        .poll(JsonInteger::new(0).unwrap(), PollWait::new(0).unwrap())
        .await
        .unwrap();
    assert_eq!(page.receipts.len(), 1);
    page.receipts.remove(0).verification
}

#[tokio::test]
async fn node_old_epoch_receipt_verifies_on_submit_poll_and_status() {
    // The current envelope is epoch 3 and the receipt names epoch 2: only a source that keeps
    // history can answer, and every receipt path must ask it for that exact row.
    assert_eq!(current_row().recipient_key_epoch.get(), 3);
    assert_eq!(historical().recipient_key_epoch.get(), 2);
    assert_eq!(receipt().binding.recipient_key_epoch.get(), 2);

    // poll
    assert!(matches!(
        poll_verification(poll_body(vec![receipt()]), false).await,
        ReceiptVerification::Verified(_)
    ));

    // status
    let server = ScriptedServer::start(vec![Reply::json(200, &status_json(&receipt()))])
        .await
        .unwrap();
    assert!(matches!(
        history_client(&server, false)
            .status(logical())
            .await
            .unwrap()
            .receipt,
        Some(ReceiptVerification::Verified(_))
    ));

    // submit: the first reply is the status carrying the epoch 2 receipt
    let server = ScriptedServer::start(vec![Reply::json(200, &status_json(&receipt()))])
        .await
        .unwrap();
    let outcome = history_client(&server, false)
        .submit(logical())
        .await
        .unwrap();
    assert!(
        !is_pending(&outcome),
        "a valid epoch 2 receipt finalizes submit: {outcome:?}"
    );
}

#[tokio::test]
async fn node_old_epoch_receipt_with_wrong_signature_or_pinned_key_is_rejected() {
    // poll
    assert!(matches!(
        poll_verification(poll_body(vec![bad_signature_receipt()]), false).await,
        ReceiptVerification::Rejected(ReceiptRejection::InvalidSignature)
    ));
    assert!(matches!(
        poll_verification(poll_body(vec![wrong_key_receipt()]), false).await,
        ReceiptVerification::Rejected(_)
    ));

    // status
    for bad in [bad_signature_receipt(), wrong_key_receipt()] {
        let server = ScriptedServer::start(vec![Reply::json(200, &status_json(&bad))])
            .await
            .unwrap();
        assert!(matches!(
            history_client(&server, false)
                .status(logical())
                .await
                .unwrap()
                .receipt,
            Some(ReceiptVerification::Rejected(_))
        ));
    }
}

#[tokio::test]
async fn node_old_epoch_receipt_source_error_is_unhandled_not_rejected() {
    // submit: an unhandled receipt is a transport error, never a finalized or rejected outcome.
    let server = ScriptedServer::start(vec![Reply::json(200, &status_json(&receipt()))])
        .await
        .unwrap();
    assert!(history_client(&server, true)
        .submit(logical())
        .await
        .is_err());

    // A failing history read says nothing about the receipt: it must stay unhandled so the
    // receipt can be read again, and must never be reported as a rejection.
    assert!(matches!(
        poll_verification(poll_body(vec![receipt()]), true).await,
        ReceiptVerification::Unhandled(khive_channel::ReceiptReadFailure::SourceUnavailable)
    ));

    let server = ScriptedServer::start(vec![Reply::json(200, &status_json(&receipt()))])
        .await
        .unwrap();
    assert!(matches!(
        history_client(&server, true)
            .status(logical())
            .await
            .unwrap()
            .receipt,
        Some(ReceiptVerification::Unhandled(
            khive_channel::ReceiptReadFailure::SourceUnavailable
        ))
    ));
}

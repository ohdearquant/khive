use crate::client::NodeClient;
use crate::client_test_server::{Reply, ScriptedServer};
use crate::encoding::*;
use crate::envelope::*;
use crate::keys::*;
use crate::pins::*;
use crate::receipt::*;
use crate::request::RequestHeaders;
use crate::response::*;
use crate::source::*;
use crate::wire::*;
use crate::ProtocolError;
use async_trait::async_trait;
use khive_channel::{ChannelError, HoldReason, PendingDetail, ReceiptDisposition, SendOutcome};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
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
fn other_id() -> CanonicalUuid {
    id("01920000-0000-7000-8000-00000000a003")
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
fn persisted() -> PersistedSubmission {
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
struct FixedSource(Result<Option<PersistedSubmission>, SourceError>);
#[async_trait]
impl OutboundSource for FixedSource {
    async fn current(&self, _logical: Uuid) -> Result<Option<PersistedSubmission>, SourceError> {
        self.0.clone()
    }
}
struct OrderedSource {
    outcomes: std::sync::Mutex<
        std::collections::VecDeque<Result<Option<PersistedSubmission>, SourceError>>,
    >,
    requested: std::sync::Mutex<Vec<Uuid>>,
}
#[async_trait]
impl OutboundSource for OrderedSource {
    async fn current(&self, logical: Uuid) -> Result<Option<PersistedSubmission>, SourceError> {
        self.requested.lock().unwrap().push(logical);
        self.outcomes
            .lock()
            .unwrap()
            .pop_front()
            .expect("each receipt must read its persisted source independently")
    }
}
struct FixedPins {
    state: PinState,
    unavailable: bool,
}
#[async_trait]
impl PinSource for FixedPins {
    async fn resolve(&self, _identity: &PinIdentity) -> Result<PinState, PinSourceError> {
        if self.unavailable {
            Err(PinSourceError("fixture read failure".into()))
        } else {
            Ok(self.state.clone())
        }
    }
}
fn pins(sender_pin: bool) -> Arc<FixedPins> {
    let keys = facility(sender_pin).public_keys();
    Arc::new(FixedPins {
        state: PinState::Confirmed(
            ConfirmedPin::new(identity(sender_pin), keys.clone(), keys.fingerprint()).unwrap(),
        ),
        unavailable: false,
    })
}
struct SpyFacility {
    inner: InMemoryKeyFacility,
    seals: AtomicUsize,
    opens: AtomicUsize,
}
impl SpyFacility {
    fn new(sender: bool) -> Arc<Self> {
        Arc::new(Self {
            inner: facility(sender),
            seals: AtomicUsize::new(0),
            opens: AtomicUsize::new(0),
        })
    }
}
impl KeyFacility for SpyFacility {
    fn public_keys(&self) -> DevicePublicKeys {
        self.inner.public_keys()
    }
    fn sign(&self, input: &[u8]) -> [u8; 64] {
        self.inner.sign(input)
    }
    fn seal(
        &self,
        header: &EnvelopeHeader,
        logical: CanonicalUuid,
        recipient: &KemPublicKey,
        plaintext: &[u8],
    ) -> Result<SealedEnvelope, ProtocolError> {
        self.seals.fetch_add(1, Ordering::SeqCst);
        self.inner.seal(header, logical, recipient, plaintext)
    }
    fn open(
        &self,
        header: &EnvelopeHeader,
        logical: CanonicalUuid,
        sender: &KemPublicKey,
        envelope: &SealedEnvelope,
    ) -> Result<Vec<u8>, ProtocolError> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        self.inner.open(header, logical, sender, envelope)
    }
}
fn client(
    server: &ScriptedServer,
    sender: bool,
    source: Arc<dyn OutboundSource>,
    pin: Arc<dyn PinSource>,
    key: Arc<dyn KeyFacility>,
) -> NodeClient {
    NodeClient::test_http(binding(sender), server.url(), source, pin, key).unwrap()
}
fn ordinary_client(server: &ScriptedServer, sender: bool) -> NodeClient {
    client(
        server,
        sender,
        Arc::new(FixedSource(Ok(Some(persisted())))),
        pins(!sender),
        SpyFacility::new(sender),
    )
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
fn admission() -> String {
    json!({"state":"pending", "delivery_attempt_id": receipt().binding.delivery_attempt_id, "admitted_at":"2026-09-24T00:00:00Z"}).to_string()
}
fn status_json(receipt: Option<&WireReceipt>) -> String {
    json!({"logical_message_id": logical(), "state":if receipt.is_some() { "recipient_stored" } else { "unknown" }, "delivery_attempt_id": receipt.map(|r| r.binding.delivery_attempt_id), "receipt": receipt}).to_string()
}
fn delivery() -> Delivery {
    let p = persisted();
    let s = binding(true);
    Delivery {
        delivery_attempt_id: receipt().binding.delivery_attempt_id,
        logical_message_id: p.logical_message_id,
        protocol_version: p.protocol_version,
        sender_agent_id: s.agent,
        sender_device_id: s.device,
        sender_key_epoch: s.key_epoch,
        recipient_agent_id: p.recipient_agent,
        recipient_device_id: p.recipient_device,
        recipient_key_epoch: p.recipient_key_epoch,
        contact_generation: p.contact_generation,
        enc: p.enc,
        ciphertext: p.ciphertext,
    }
}
fn contact_response(keys: DevicePublicKeys) -> ContactResponse {
    let b = binding(false);
    ContactResponse {
        agent_id: b.agent,
        address: NodeAddress::new(b.realm, b.agent),
        device_id: b.device,
        key_epoch: b.key_epoch,
        kem_public_key: keys.kem.clone(),
        signing_public_key: keys.signing.clone(),
        fingerprint: keys.fingerprint(),
        contact_generation: Epoch::new(3).unwrap(),
    }
}
fn poll_body(raw: Option<&str>, receipts: Vec<WireReceipt>) -> String {
    let items: Vec<Value> = receipts
        .into_iter()
        .enumerate()
        .map(|(i, r)| json!({"seq":i+1,"receipt":r,"recorded_at":"2026-09-24T00:00:00Z"}))
        .collect();
    format!("{{\"deliveries\":[{}],\"receipts\":{},\"receipts_cursor\":{},\"server_time\":\"2026-09-24T00:00:00Z\"}}", raw.unwrap_or(""), serde_json::to_string(&items).unwrap(),items.len())
}
fn is_pending(outcome: &SendOutcome) -> bool {
    matches!(
        outcome,
        SendOutcome::Pending(PendingDetail::ReceiptUnverified { .. })
    )
}

#[tokio::test]
async fn a11_http_body_and_signature_are_exact() {
    let server = ScriptedServer::start(vec![Reply::json(202, &admission())])
        .await
        .unwrap();
    let c = ordinary_client(&server, true);
    c.test_auth(vec![(
        value("request_authentication", "timestamp")
            .parse()
            .unwrap(),
        decode_hex(&value("request_authentication", "nonce"))
            .unwrap()
            .try_into()
            .unwrap(),
    )]);
    assert!(matches!(
        c.submit(logical()).await.unwrap(),
        SendOutcome::Pending(PendingDetail::Admitted { .. })
    ));
    let captured = server.requests().await;
    let req = &captured[0];
    assert_eq!(req.body, value("request_body_json", "utf8").as_bytes());
    assert_eq!(req.method, "POST");
    assert_eq!(req.target, "/node/v1/messages");
    let h = RequestHeaders::parse(
        req.header("khive-device").unwrap(),
        req.header("khive-timestamp").unwrap(),
        req.header("khive-nonce").unwrap(),
        req.header("khive-signature").unwrap(),
    )
    .unwrap();
    assert_eq!(
        h.signature.as_bytes().as_slice(),
        decode_hex(&value("request_authentication", "signature")).unwrap()
    );
    h.verify(
        &facility(true).public_keys().signing,
        &req.method,
        &req.target,
        &req.body,
    )
    .unwrap();
}

#[tokio::test]
async fn submit_a9_outcomes_are_exact() {
    for (status, code, expected) in [
        (400, "invalid_request", "permanent"),
        (400, "unsupported_version", "permanent"),
        (401, "unauthenticated", "auth"),
        (402, "insufficient_credit", "held"),
        (403, "contact_not_active", "permanent"),
        (409, "recipient_offline", "transport"),
        (409, "recipient_key_changed", "held"),
        (409, "envelope_conflict", "permanent"),
        (409, "receipt_conflict", "permanent"),
        (409, "poll_in_progress", "transport"),
        (413, "payload_too_large", "permanent"),
        (422, "receipt_invalid", "permanent"),
        (429, "rate_limited", "transport"),
        (503, "capacity_exhausted", "transport"),
        (400, "future_code", "permanent"),
    ] {
        let server = ScriptedServer::start(vec![Reply::json(
            status,
            &format!("{{\"error\":\"{code}\",\"retry_after_seconds\":31}}"),
        )])
        .await
        .unwrap();
        let outcome = ordinary_client(&server, true).submit(logical()).await;
        match expected {
            "held" => {
                let reason = if code == "insufficient_credit" {
                    HoldReason::InsufficientCredit
                } else {
                    HoldReason::RecipientKeyChanged
                };
                assert!(matches!(
                    outcome,
                    Ok(SendOutcome::Pending(PendingDetail::Held(actual))) if actual == reason
                ));
            }
            "auth" => assert!(matches!(
                outcome.unwrap_err().channel_error(),
                Some(ChannelError::Auth(_))
            )),
            "transport" => assert!(matches!(
                outcome.unwrap_err().channel_error(),
                Some(ChannelError::Transport(_))
            )),
            _ => assert!(matches!(
                outcome.unwrap_err().channel_error(),
                Some(ChannelError::PermanentTransport(_))
            )),
        }
        assert_eq!(server.requests().await.len(), 1, "{code}");
    }
    for status in [202, 200] {
        let server = ScriptedServer::start(vec![Reply::json(status, &admission())])
            .await
            .unwrap();
        match ordinary_client(&server, true)
            .submit(logical())
            .await
            .unwrap()
        {
            SendOutcome::Pending(PendingDetail::Admitted { admitted_at }) => assert_eq!(
                admitted_at,
                UtcTimestamp::parse("2026-09-24T00:00:00Z")
                    .unwrap()
                    .as_utc()
            ),
            other => panic!("expected admission for HTTP {status}, got {other:?}"),
        }
    }
    for disposition in [ReceiptDisposition::Stored, ReceiptDisposition::Quarantined] {
        let r = WireReceipt::sign(receipt().binding, disposition, &facility(false)).unwrap();
        let server = ScriptedServer::start(vec![Reply::json(200, &status_json(Some(&r)))])
            .await
            .unwrap();
        let result = ordinary_client(&server, true)
            .submit(logical())
            .await
            .unwrap();
        assert!(matches!(
            (disposition, result),
            (ReceiptDisposition::Stored, SendOutcome::RecipientStored(_))
                | (
                    ReceiptDisposition::Quarantined,
                    SendOutcome::RecipientQuarantined(_)
                )
        ));
    }
}

#[tokio::test]
async fn server_failures_and_malformed_success_never_claim_receipts() {
    for (status, body) in [
        (503, "not JSON"),
        (503, "{\"error\":\"envelope_conflict\"}"),
        (200, "{\"state\":\"recipient_stored\"}"),
        (202, "{\"state\":\"pending\"}"),
    ] {
        let server = ScriptedServer::start(vec![Reply::json(status, body)])
            .await
            .unwrap();
        assert!(
            matches!(
                ordinary_client(&server, true)
                    .submit(logical())
                    .await
                    .unwrap_err()
                    .channel_error(),
                Some(ChannelError::Transport(_))
            ),
            "HTTP {status} with {body} cannot prove admission or receipt"
        );
        assert_eq!(server.requests().await.len(), 1);
    }
}

#[tokio::test]
async fn node_401_pauses_with_clock_diagnostic() {
    for (offset, expected) in [
        (-301, ClockDiagnosis::MoreThan300SecondsBehind),
        (-300, ClockDiagnosis::WithinSkewBounds),
        (59, ClockDiagnosis::WithinSkewBounds),
        (60, ClockDiagnosis::AtLeast60SecondsAhead),
    ] {
        let timestamp: u64 = 1_790_193_600;
        let mut reply = Reply::json(401, "{}");
        reply.headers.push((
            "Date".into(),
            httpdate::fmt_http_date(std::time::UNIX_EPOCH + Duration::from_secs(timestamp)),
        ));
        let server = ScriptedServer::start(vec![reply]).await.unwrap();
        let c = ordinary_client(&server, true);
        c.test_auth(vec![((timestamp as i64 + offset) as u64, [3; 16])]);
        match c.submit(logical()).await.unwrap_err() {
            NodeError::Channel {
                error: ChannelError::Auth(_),
                diagnostic: Some(d),
            } => {
                assert_eq!(d.clock, Some(expected));
                assert!(d.date.is_some());
            }
            other => panic!("expected node Auth, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn node_401_missing_or_malformed_date_remains_auth() {
    for date in [None, Some("not an HTTP date")] {
        let mut reply = Reply::json(401, "not JSON");
        if let Some(date) = date {
            reply.headers.push(("Date".into(), date.into()));
        }
        let server = ScriptedServer::start(vec![reply]).await.unwrap();
        let c = ordinary_client(&server, true);
        c.test_auth(vec![(1_790_193_600, [3; 16])]);
        match c.submit(logical()).await.unwrap_err() {
            NodeError::Channel {
                error: ChannelError::Auth(_),
                diagnostic: Some(d),
            } => {
                assert_eq!(d.status, 401);
                assert_eq!(d.date.as_deref(), date);
                assert_eq!(d.clock, Some(ClockDiagnosis::DateUnavailable));
            }
            other => panic!("expected node Auth without usable Date, got {other:?}"),
        }
        assert_eq!(server.requests().await.len(), 1);
    }
}

fn changed_receipts() -> Vec<WireReceipt> {
    let original = receipt();
    let mut changed = Vec::new();
    for field in 0..7 {
        let mut r = original.clone();
        match field {
            0 => r.binding.logical_message_id = other_id(),
            1 => r.binding.sender_agent_id = other_id(),
            2 => r.binding.recipient_agent_id = other_id(),
            3 => r.binding.recipient_device_id = other_id(),
            4 => r.binding.recipient_key_epoch = Epoch::new(3).unwrap(),
            5 => r.binding.contact_generation = Epoch::new(4).unwrap(),
            _ => r.binding.delivery_attempt_id = other_id(),
        };
        changed.push(r);
    }
    changed
}
#[tokio::test]
async fn submit_bad_binding_stays_pending() {
    for (field, mut r) in [
        "logical_message_id",
        "sender_agent_id",
        "recipient_agent_id",
        "recipient_device_id",
        "recipient_key_epoch",
        "contact_generation",
    ]
    .into_iter()
    .zip(changed_receipts().into_iter().take(6))
    {
        r = WireReceipt::sign(r.binding, r.disposition, &facility(false)).unwrap();
        r.verify(&facility(false).public_keys().signing).unwrap();
        let server = ScriptedServer::start(vec![Reply::json(200, &status_json(Some(&r)))])
            .await
            .unwrap();
        assert!(
            matches!(ordinary_client(&server, true).submit(logical()).await.unwrap(),
                SendOutcome::Pending(PendingDetail::ReceiptUnverified { reason })
                    if reason == "BindingMismatch"),
            "validly signed conflicting {field} must stay pending"
        );
        assert_eq!(server.requests().await.len(), 1, "{field}");
    }
    let mut body: Value = serde_json::from_str(&status_json(Some(&receipt()))).unwrap();
    body["receipt"]["binding"]["protocol_version"] = json!(2);
    let server = ScriptedServer::start(vec![Reply::json(200, &body.to_string())])
        .await
        .unwrap();
    assert!(
        matches!(
            ordinary_client(&server, true)
                .submit(logical())
                .await
                .unwrap_err()
                .channel_error(),
            Some(ChannelError::Transport(_))
        ),
        "unsupported protocol is refused during typed decoding"
    );
}

#[tokio::test]
async fn submit_accepts_a_new_canonical_service_attempt_but_refuses_noncanonical_attempt() {
    let mut r = receipt();
    r.binding.delivery_attempt_id = other_id();
    r = WireReceipt::sign(r.binding, r.disposition, &facility(false)).unwrap();
    let server = ScriptedServer::start(vec![Reply::json(200, &status_json(Some(&r)))])
        .await
        .unwrap();
    match ordinary_client(&server, true)
        .submit(logical())
        .await
        .unwrap()
    {
        SendOutcome::RecipientStored(accepted) => {
            assert_eq!(
                accepted.receipt().binding.delivery_attempt_id,
                other_id().into_uuid()
            );
        }
        other => panic!("canonical service attempt must be accepted, got {other:?}"),
    }
    for spelling in [
        other_id().to_string().to_uppercase(),
        other_id().to_string().replace('-', ""),
        "not-an-attempt-id".into(),
    ] {
        let mut body: Value = serde_json::from_str(&status_json(Some(&r))).unwrap();
        body["receipt"]["binding"]["delivery_attempt_id"] = json!(spelling);
        let server = ScriptedServer::start(vec![Reply::json(200, &body.to_string())])
            .await
            .unwrap();
        assert!(
            matches!(
                ordinary_client(&server, true)
                    .submit(logical())
                    .await
                    .unwrap_err()
                    .channel_error(),
                Some(ChannelError::Transport(_))
            ),
            "attempt IDs must use their canonical wire spelling"
        );
    }
}
#[tokio::test]
async fn submit_uses_pinned_receipt_key() {
    let alternate = facility(true);
    let r = WireReceipt::sign(receipt().binding, ReceiptDisposition::Stored, &alternate).unwrap();
    let keys = alternate.public_keys();
    let target = binding(false);
    let observed = ContactResponse {
        agent_id: target.agent,
        address: NodeAddress::new(target.realm, target.agent),
        device_id: target.device,
        key_epoch: target.key_epoch,
        kem_public_key: keys.kem.clone(),
        signing_public_key: keys.signing.clone(),
        fingerprint: keys.fingerprint(),
        contact_generation: Epoch::new(3).unwrap(),
    };
    let server = ScriptedServer::start(vec![
        Reply::json(200, &status_json(Some(&r))),
        Reply::json(200, &serde_json::to_string(&observed).unwrap()),
    ])
    .await
    .unwrap();
    assert!(
        is_pending(
            &ordinary_client(&server, true)
                .submit(logical())
                .await
                .unwrap()
        ),
        "a service-supplied key cannot finalize receipt"
    );
}

#[tokio::test]
async fn ack_not_found_is_done() {
    let server = ScriptedServer::start(vec![Reply::json(404, "{\"error\":\"not_found\"}")])
        .await
        .unwrap();
    assert!(ordinary_client(&server, false)
        .ack(&receipt())
        .await
        .is_ok());
}
#[tokio::test]
async fn ack_a9_outcomes_are_exact() {
    for (status, code, expected) in [
        (401, "unauthenticated", "auth"),
        (409, "receipt_conflict", "permanent"),
        (422, "receipt_invalid", "permanent"),
        (400, "invalid_request", "permanent"),
        (413, "payload_too_large", "permanent"),
        (429, "rate_limited", "transport"),
        (503, "capacity_exhausted", "transport"),
    ] {
        let server = ScriptedServer::start(vec![Reply::json(
            status,
            &format!("{{\"error\":\"{code}\"}}"),
        )])
        .await
        .unwrap();
        let result = ordinary_client(&server, false)
            .ack(&receipt())
            .await
            .unwrap_err();
        match expected {
            "auth" => assert!(matches!(
                result.channel_error(),
                Some(ChannelError::Auth(_))
            )),
            "transport" => assert!(matches!(
                result.channel_error(),
                Some(ChannelError::Transport(_))
            )),
            _ => assert!(matches!(
                result.channel_error(),
                Some(ChannelError::PermanentTransport(_))
            )),
        }
    }
    let server = ScriptedServer::start(vec![Reply::json(200, "{\"recorded\":true}")])
        .await
        .unwrap();
    assert!(ordinary_client(&server, false)
        .ack(&receipt())
        .await
        .is_ok());
}
#[tokio::test]
async fn ack_refuses_every_altered_signed_binding() {
    let server = ScriptedServer::start(vec![]).await.unwrap();
    let c = ordinary_client(&server, false);
    for r in changed_receipts() {
        assert!(
            matches!(
                c.ack(&r).await.unwrap_err().channel_error(),
                Some(ChannelError::InvalidEnvelope(_))
            ),
            "each signed binding alteration must refuse before HTTP"
        );
    }
    let mut unsupported = serde_json::to_value(receipt()).unwrap();
    unsupported["binding"]["protocol_version"] = json!(2);
    assert!(serde_json::from_value::<WireReceipt>(unsupported).is_err());
    let mut r = receipt();
    r.disposition = ReceiptDisposition::Quarantined;
    assert!(c.ack(&r).await.is_err());
    assert!(server.requests().await.is_empty());
}

#[tokio::test]
async fn poll_rejects_first_receipt_and_processes_later() {
    let mut bad = receipt();
    bad.signature = Base64Bytes::new([0; 64]);
    let server = ScriptedServer::start(vec![Reply::json(
        200,
        &poll_body(None, vec![bad, receipt()]),
    )])
    .await
    .unwrap();
    let page = ordinary_client(&server, true)
        .poll(JsonInteger::new(0).unwrap(), PollWait::new(0).unwrap())
        .await
        .unwrap();
    assert_eq!(page.next_cursor.get(), 2);
    assert!(matches!(
        page.receipts[0].verification,
        ReceiptVerification::Rejected(_)
    ));
    assert!(matches!(
        page.receipts[1].verification,
        ReceiptVerification::Verified(_)
    ));
    assert!(page.deliveries.is_empty());
    assert_eq!(
        server.requests().await.len(),
        1,
        "polling never acknowledges"
    );
}

#[tokio::test]
async fn poll_source_failures_do_not_block_later_receipts_or_cursor() {
    let expected = receipt();
    let source = Arc::new(OrderedSource {
        outcomes: std::sync::Mutex::new(
            vec![
                Err(SourceError("fixture read failure".into())),
                Ok(None),
                Ok(Some(persisted())),
            ]
            .into(),
        ),
        requested: std::sync::Mutex::new(Vec::new()),
    });
    let server = ScriptedServer::start(vec![Reply::json(
        200,
        &poll_body(
            None,
            vec![expected.clone(), expected.clone(), expected.clone()],
        ),
    )])
    .await
    .unwrap();
    let c = client(
        &server,
        true,
        source.clone(),
        pins(false),
        SpyFacility::new(true),
    );
    let page = c
        .poll(JsonInteger::new(0).unwrap(), PollWait::new(0).unwrap())
        .await
        .unwrap();
    assert_eq!(page.next_cursor.get(), 3);
    assert_eq!(page.receipts.len(), 3);
    for (index, reason) in [
        ReceiptRejection::SourceUnavailable,
        ReceiptRejection::SourceMissing,
    ]
    .into_iter()
    .enumerate()
    {
        assert!(matches!(&page.receipts[index].verification,
            ReceiptVerification::Rejected(actual) if actual == &reason));
        assert_eq!(page.receipts[index].item.seq.get(), index as u64 + 1);
    }
    match &page.receipts[2].verification {
        ReceiptVerification::Verified(verified) => assert_eq!(verified.receipt(), &expected),
        other => panic!("later valid receipt must be independently processed, got {other:?}"),
    }
    assert_eq!(page.receipts[2].item.seq.get(), 3);
    assert_eq!(*source.requested.lock().unwrap(), vec![logical(); 3]);
    assert!(source.outcomes.lock().unwrap().is_empty());
    assert_eq!(
        server.requests().await.len(),
        1,
        "poll does not acknowledge receipts"
    );
}
#[tokio::test]
async fn poll_unconfirmed_sender_stays_unopened() {
    let raw = serde_json::to_string(&delivery()).unwrap();
    let server = ScriptedServer::start(vec![Reply::json(200, &poll_body(Some(&raw), vec![]))])
        .await
        .unwrap();
    let keys = SpyFacility::new(false);
    let p = Arc::new(FixedPins {
        state: PinState::UnconfirmedEpoch {
            candidate: Some(UnconfirmedPin {
                identity: identity(true),
                keys: facility(true).public_keys(),
            }),
        },
        unavailable: false,
    });
    let c = client(
        &server,
        false,
        Arc::new(FixedSource(Ok(None))),
        p,
        keys.clone(),
    );
    let page = c
        .poll(JsonInteger::new(0).unwrap(), PollWait::new(0).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        page.deliveries[0].opening(),
        DeliveryOpenResult::HeldUnopened(HeldReason::UnconfirmedSenderEpoch)
    ));
    assert_eq!(
        keys.opens.load(Ordering::SeqCst),
        0,
        "unconfirmed epoch must never reach open"
    );
    assert!(page.deliveries[0].receipt_binding().is_err());
}

#[tokio::test]
async fn poll_pin_read_failure_is_explicitly_held_without_opening() {
    let raw = serde_json::to_string(&delivery()).unwrap();
    let server = ScriptedServer::start(vec![Reply::json(200, &poll_body(Some(&raw), vec![]))])
        .await
        .unwrap();
    let keys = SpyFacility::new(false);
    let p = Arc::new(FixedPins {
        state: pins(true).state.clone(),
        unavailable: true,
    });
    let c = client(
        &server,
        false,
        Arc::new(FixedSource(Ok(None))),
        p,
        keys.clone(),
    );
    let page = c
        .poll(JsonInteger::new(0).unwrap(), PollWait::new(0).unwrap())
        .await
        .unwrap();
    assert_eq!(page.deliveries.len(), 1);
    assert_eq!(page.deliveries[0].original_json(), raw.as_bytes());
    assert!(matches!(
        page.deliveries[0].opening(),
        DeliveryOpenResult::HeldUnopened(HeldReason::PinUnavailable)
    ));
    assert_eq!(keys.opens.load(Ordering::SeqCst), 0);
    assert!(page.deliveries[0].receipt_binding().is_err());
}

#[tokio::test]
async fn poll_fingerprint_mismatch_stays_unopened() {
    for state in [
        PinState::FingerprintMismatch,
        PinState::Confirmed(
            ConfirmedPin::new(
                identity(false),
                facility(true).public_keys(),
                facility(true).public_keys().fingerprint(),
            )
            .unwrap(),
        ),
    ] {
        let raw = serde_json::to_string(&delivery()).unwrap();
        let server = ScriptedServer::start(vec![Reply::json(200, &poll_body(Some(&raw), vec![]))])
            .await
            .unwrap();
        let keys = SpyFacility::new(false);
        let p = Arc::new(FixedPins {
            state,
            unavailable: false,
        });
        let c = client(
            &server,
            false,
            Arc::new(FixedSource(Ok(None))),
            p,
            keys.clone(),
        );
        let page = c
            .poll(JsonInteger::new(0).unwrap(), PollWait::new(0).unwrap())
            .await
            .unwrap();
        assert!(matches!(
            page.deliveries[0].opening(),
            DeliveryOpenResult::HeldUnopened(HeldReason::FingerprintMismatch)
        ));
        assert_eq!(keys.opens.load(Ordering::SeqCst), 0);
        assert!(page.deliveries[0].receipt_binding().is_err());
    }
}
#[tokio::test]
async fn poll_wrong_recipient_and_noncontact_stay_unopened() {
    for field in 0..4 {
        let mut d = delivery();
        if field == 0 {
            d.recipient_agent_id = other_id();
        } else if field == 1 {
            d.recipient_device_id = other_id();
        } else if field == 2 {
            d.recipient_key_epoch = Epoch::new(3).unwrap();
        }
        let raw = serde_json::to_string(&d).unwrap();
        let server = ScriptedServer::start(vec![Reply::json(200, &poll_body(Some(&raw), vec![]))])
            .await
            .unwrap();
        let keys = SpyFacility::new(false);
        let p: Arc<dyn PinSource> = if field == 3 {
            Arc::new(FixedPins {
                state: PinState::NonContact,
                unavailable: false,
            })
        } else {
            pins(true)
        };
        let c = client(
            &server,
            false,
            Arc::new(FixedSource(Ok(None))),
            p,
            keys.clone(),
        );
        let page = c
            .poll(JsonInteger::new(0).unwrap(), PollWait::new(0).unwrap())
            .await
            .unwrap();
        assert!(matches!(
            page.deliveries[0].opening(),
            DeliveryOpenResult::HeldUnopened(_)
        ));
        assert_eq!(keys.opens.load(Ordering::SeqCst), 0);
    }
}
#[tokio::test]
async fn poll_failed_open_has_no_opened_result() {
    let mut d = delivery();
    let mut bytes = d.ciphertext.as_bytes().to_vec();
    *bytes.last_mut().unwrap() ^= 1;
    d.ciphertext = Ciphertext::new(bytes).unwrap();
    let raw = serde_json::to_string(&d).unwrap();
    let server = ScriptedServer::start(vec![Reply::json(200, &poll_body(Some(&raw), vec![]))])
        .await
        .unwrap();
    let page = ordinary_client(&server, false)
        .poll(JsonInteger::new(0).unwrap(), PollWait::new(0).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        page.deliveries[0].opening(),
        DeliveryOpenResult::HeldUnopened(HeldReason::EnvelopeDidNotAuthenticate)
    ));
    assert!(page.deliveries[0].receipt_binding().is_err());
}
#[tokio::test]
async fn poll_preserves_original_delivery_object_bytes() {
    // Escape only the leading zero; the decoded canonical UUID stays unchanged.
    let raw = serde_json::to_string_pretty(&serde_json::to_value(delivery()).unwrap())
        .unwrap()
        .replace("01920000", "\\u00301920000");
    assert_eq!(serde_json::from_str::<Delivery>(&raw).unwrap(), delivery());
    let server = ScriptedServer::start(vec![Reply::json(200, &poll_body(Some(&raw), vec![]))])
        .await
        .unwrap();
    let page = ordinary_client(&server, false)
        .poll(JsonInteger::new(0).unwrap(), PollWait::new(0).unwrap())
        .await
        .unwrap();
    let item = &page.deliveries[0];
    assert!(matches!(item.opening(), DeliveryOpenResult::Opened(_)));
    assert_eq!(
        item.original_json(),
        raw.as_bytes(),
        "original delivery slice must remain verbatim"
    );
    assert_ne!(
        item.original_json(),
        serde_json::to_vec(item.delivery()).unwrap()
    );
    let binding = item.receipt_binding().unwrap();
    assert_eq!(binding, receipt().binding);
    let signed = WireReceipt::sign(binding, ReceiptDisposition::Stored, &facility(false)).unwrap();
    assert_eq!(
        signed.signing_input().unwrap(),
        decode_hex(&value("receipt_stored", "signing_input")).unwrap()
    );
    assert_eq!(signed.signature, receipt().signature);
}

#[tokio::test]
async fn submit_requires_persisted_envelope() {
    let server = ScriptedServer::start(vec![]).await.unwrap();
    let keys = SpyFacility::new(true);
    let c = client(
        &server,
        true,
        Arc::new(FixedSource(Ok(None))),
        pins(false),
        keys.clone(),
    );
    assert!(matches!(
        c.submit(logical()).await.unwrap_err().channel_error(),
        Some(ChannelError::InvalidEnvelope(_))
    ));
    assert!(server.requests().await.is_empty());
    assert_eq!(
        keys.seals.load(Ordering::SeqCst),
        0,
        "source absence must never seal a fresh envelope"
    );
}
#[tokio::test]
async fn submit_rejects_source_binding_mismatch() {
    for field in 0..10 {
        let mut p = persisted();
        match field {
            0 => p.namespace = "elsewhere".into(),
            1 => p.kind = "email".into(),
            2 => p.slug = "other".into(),
            3 => p.key_reference = "other-key".into(),
            4 => p.sender_agent = other_id(),
            5 => p.sender_device = other_id(),
            6 => p.sender_key_epoch = Epoch::new(3).unwrap(),
            7 => p.logical_message_id = other_id(),
            8 => p.recipient_agent = other_id(),
            _ => {
                p.recipient =
                    NodeAddress::new(Realm::parse("other.example").unwrap(), p.recipient_agent)
            }
        };
        let server = ScriptedServer::start(vec![]).await.unwrap();
        let c = client(
            &server,
            true,
            Arc::new(FixedSource(Ok(Some(p)))),
            pins(false),
            SpyFacility::new(true),
        );
        assert!(matches!(
            c.submit(logical()).await.unwrap_err().channel_error(),
            Some(ChannelError::InvalidEnvelope(_))
        ));
        assert!(server.requests().await.is_empty());
    }
}
#[tokio::test]
async fn source_errors_are_distinct_from_absence() {
    let server = ScriptedServer::start(vec![]).await.unwrap();
    let c = client(
        &server,
        true,
        Arc::new(FixedSource(Err(SourceError("fixture".into())))),
        pins(false),
        SpyFacility::new(true),
    );
    assert!(matches!(
        c.submit(logical()).await,
        Err(NodeError::Source(_))
    ));
    assert!(server.requests().await.is_empty());
}

#[tokio::test]
async fn contact_fingerprint_mismatch_does_not_establish_trust() {
    let alternate = facility(true).public_keys();
    let observed = contact_response(alternate);
    let r = WireReceipt::sign(
        receipt().binding,
        ReceiptDisposition::Stored,
        &facility(true),
    )
    .unwrap();
    let server = ScriptedServer::start(vec![
        Reply::json(200, &serde_json::to_string(&observed).unwrap()),
        Reply::json(200, &status_json(Some(&r))),
    ])
    .await
    .unwrap();
    let keys = SpyFacility::new(true);
    let c = client(
        &server,
        true,
        Arc::new(FixedSource(Ok(Some(persisted())))),
        pins(false),
        keys.clone(),
    );
    let result = c.contact(binding(false).agent.into_uuid()).await.unwrap();
    assert_eq!(result.observed, observed);
    assert!(
        !result.matches_owner_pin,
        "directory consistency does not replace an owner pin"
    );
    assert!(matches!(c.submit(logical()).await.unwrap(),
        SendOutcome::Pending(PendingDetail::ReceiptUnverified { reason })
            if reason == "InvalidSignature"));
    assert_eq!(keys.seals.load(Ordering::SeqCst), 0);
    assert_eq!(keys.opens.load(Ordering::SeqCst), 0);
    assert_eq!(server.requests().await.len(), 2);
}

#[tokio::test]
async fn contact_lookup_does_not_confirm_untrusted_keys() {
    let observed = contact_response(facility(false).public_keys());
    for state in [
        PinState::NonContact,
        PinState::UnconfirmedEpoch {
            candidate: Some(UnconfirmedPin {
                identity: identity(false),
                keys: facility(false).public_keys(),
            }),
        },
        PinState::FingerprintMismatch,
    ] {
        let server = ScriptedServer::start(vec![
            Reply::json(200, &serde_json::to_string(&observed).unwrap()),
            Reply::json(200, &status_json(Some(&receipt()))),
        ])
        .await
        .unwrap();
        let p = Arc::new(FixedPins {
            state: state.clone(),
            unavailable: false,
        });
        let c = client(
            &server,
            true,
            Arc::new(FixedSource(Ok(Some(persisted())))),
            p,
            SpyFacility::new(true),
        );
        assert!(
            !c.contact(binding(false).agent.into_uuid())
                .await
                .unwrap()
                .matches_owner_pin
        );
        let reason = match state {
            PinState::FingerprintMismatch => "FingerprintMismatch",
            _ => "PinUnconfirmed",
        };
        assert!(matches!(c.submit(logical()).await.unwrap(),
            SendOutcome::Pending(PendingDetail::ReceiptUnverified { reason: actual })
                if actual == reason));
        assert_eq!(server.requests().await.len(), 2);
    }
}

#[tokio::test]
async fn contact_inconsistent_fingerprint_and_pin_read_failure_are_distinct() {
    let mut observed = contact_response(facility(false).public_keys());
    observed.fingerprint = facility(true).public_keys().fingerprint();
    let server = ScriptedServer::start(vec![Reply::json(
        200,
        &serde_json::to_string(&observed).unwrap(),
    )])
    .await
    .unwrap();
    assert!(matches!(
        ordinary_client(&server, true)
            .contact(binding(false).agent.into_uuid())
            .await
            .unwrap_err()
            .channel_error(),
        Some(ChannelError::Transport(_))
    ));

    let observed = contact_response(facility(false).public_keys());
    let server = ScriptedServer::start(vec![Reply::json(
        200,
        &serde_json::to_string(&observed).unwrap(),
    )])
    .await
    .unwrap();
    let p = Arc::new(FixedPins {
        state: pins(false).state.clone(),
        unavailable: true,
    });
    let c = client(
        &server,
        true,
        Arc::new(FixedSource(Ok(Some(persisted())))),
        p,
        SpyFacility::new(true),
    );
    assert!(matches!(
        c.contact(binding(false).agent.into_uuid()).await,
        Err(NodeError::Pins(_))
    ));
}

#[tokio::test]
async fn poll_refuses_noncanonical_attempt_before_opening() {
    for spelling in [
        other_id().to_string().to_uppercase(),
        other_id().to_string().replace('-', ""),
        "not-an-attempt-id".into(),
    ] {
        let mut d = serde_json::to_value(delivery()).unwrap();
        d["delivery_attempt_id"] = json!(spelling);
        let raw = d.to_string();
        let server = ScriptedServer::start(vec![Reply::json(200, &poll_body(Some(&raw), vec![]))])
            .await
            .unwrap();
        let keys = SpyFacility::new(false);
        let c = client(
            &server,
            false,
            Arc::new(FixedSource(Ok(None))),
            pins(true),
            keys.clone(),
        );
        let page = c
            .poll(JsonInteger::new(0).unwrap(), PollWait::new(0).unwrap())
            .await
            .expect("a malformed delivery must not discard a valid page");
        assert!(page.deliveries.is_empty());
        assert!(page.receipts.is_empty());
        assert_eq!(page.next_cursor.get(), 0);
        assert_eq!(keys.opens.load(Ordering::SeqCst), 0);
        let requests = server.requests().await;
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].target, "/node/v1/poll?wait=0&receipts_after=0");
    }
}
#[tokio::test]
async fn retry_body_is_identical_and_authentication_is_fresh() {
    let server = ScriptedServer::start(vec![
        Reply::json(
            503,
            "{\"error\":\"capacity_exhausted\",\"retry_after_seconds\":60}",
        ),
        Reply::json(202, &admission()),
    ])
    .await
    .unwrap();
    let c = ordinary_client(&server, true);
    assert!(c.submit(logical()).await.is_err());
    assert!(c.submit(logical()).await.is_ok());
    let req = server.requests().await;
    assert_eq!(req.len(), 2);
    assert_eq!(req[0].body, req[1].body);
    assert_ne!(req[0].header("khive-nonce"), req[1].header("khive-nonce"));
    for r in req {
        RequestHeaders::parse(
            r.header("khive-device").unwrap(),
            r.header("khive-timestamp").unwrap(),
            r.header("khive-nonce").unwrap(),
            r.header("khive-signature").unwrap(),
        )
        .unwrap()
        .verify(
            &facility(true).public_keys().signing,
            &r.method,
            &r.target,
            &r.body,
        )
        .unwrap();
    }
}
#[tokio::test]
async fn requests_never_send_authorization() {
    let keys = facility(false).public_keys();
    let b = binding(false);
    let contact = ContactResponse {
        agent_id: b.agent,
        address: NodeAddress::new(b.realm, b.agent),
        device_id: b.device,
        key_epoch: b.key_epoch,
        kem_public_key: keys.kem.clone(),
        signing_public_key: keys.signing.clone(),
        fingerprint: keys.fingerprint(),
        contact_generation: Epoch::new(3).unwrap(),
    };
    let server = ScriptedServer::start(vec![
        Reply::json(200, &serde_json::to_string(&contact).unwrap()),
        Reply::json(200, &status_json(None)),
        Reply::json(202, &admission()),
        Reply::json(200, &poll_body(None, vec![])),
        Reply::json(200, "{\"recorded\":true}"),
    ])
    .await
    .unwrap();
    let c = ordinary_client(&server, true);
    assert!(
        c.contact(binding(false).agent.into_uuid())
            .await
            .unwrap()
            .matches_owner_pin
    );
    assert_eq!(
        c.status(logical()).await.unwrap().state,
        MessageState::Unknown
    );
    c.submit(logical()).await.unwrap();
    c.poll(JsonInteger::new(0).unwrap(), PollWait::new(0).unwrap())
        .await
        .unwrap();
    ordinary_client(&server, false)
        .ack(&receipt())
        .await
        .unwrap();
    let req = server.requests().await;
    assert_eq!(req.len(), 5);
    let expected = [
        ("GET", format!("/node/v1/contacts/{}", binding(false).agent)),
        ("GET", format!("/node/v1/messages/{}", logical())),
        ("POST", "/node/v1/messages".into()),
        ("GET", "/node/v1/poll?wait=0&receipts_after=0".into()),
        ("POST", "/node/v1/receipts".into()),
    ];
    for (index, r) in req.into_iter().enumerate() {
        assert!(r.header("authorization").is_none());
        assert_eq!(r.method, expected[index].0);
        assert_eq!(r.target, expected[index].1);
        for name in [
            "khive-device",
            "khive-timestamp",
            "khive-nonce",
            "khive-signature",
        ] {
            assert!(r.header(name).is_some());
        }
        RequestHeaders::parse(
            r.header("khive-device").unwrap(),
            r.header("khive-timestamp").unwrap(),
            r.header("khive-nonce").unwrap(),
            r.header("khive-signature").unwrap(),
        )
        .unwrap()
        .verify(
            &facility(index != 4).public_keys().signing,
            &r.method,
            &r.target,
            &r.body,
        )
        .unwrap();
    }
}

#[tokio::test]
async fn requests_do_not_follow_redirects() {
    let target = ScriptedServer::start(vec![Reply::json(202, &admission())])
        .await
        .unwrap();
    let mut redirect = Reply::json(307, "{}");
    redirect.headers.push((
        "Location".into(),
        format!("{}/node/v1/messages", target.url()),
    ));
    let server = ScriptedServer::start(vec![redirect]).await.unwrap();
    assert!(matches!(
        ordinary_client(&server, true)
            .submit(logical())
            .await
            .unwrap_err()
            .channel_error(),
        Some(ChannelError::Transport(_))
    ));
    assert_eq!(server.requests().await.len(), 1);
    assert!(
        target.requests().await.is_empty(),
        "a signed target must not be redirected"
    );
}
#[tokio::test]
async fn poll_calls_do_not_overlap_and_timeout_exceeds_wait() {
    assert!(crate::client::REQUEST_TIMEOUT > Duration::from_secs(25));
    let gate = Arc::new(tokio::sync::Notify::new());
    let mut first = Reply::json(200, &poll_body(None, vec![]));
    first.gate = Some(gate.clone());
    let server = ScriptedServer::start(vec![first, Reply::json(200, &poll_body(None, vec![]))])
        .await
        .unwrap();
    let c = ordinary_client(&server, true);
    let first = c.clone();
    let a = tokio::spawn(async move {
        first
            .poll(JsonInteger::new(0).unwrap(), PollWait::new(25).unwrap())
            .await
    });
    server.wait_for_requests(1).await.unwrap();
    let second = c.clone();
    let b = tokio::spawn(async move {
        second
            .poll(JsonInteger::new(0).unwrap(), PollWait::new(0).unwrap())
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(100), server.wait_for_requests(2))
            .await
            .is_err(),
        "second poll cannot start while first response is gated"
    );
    gate.notify_one();
    a.await.unwrap().unwrap();
    b.await.unwrap().unwrap();
    server.wait_for_requests(2).await.unwrap();
    assert_eq!(
        server.requests().await[0].target,
        "/node/v1/poll?wait=25&receipts_after=0"
    );
}
#[tokio::test]
async fn response_and_delivery_limits_are_bounded() {
    let body = format!("{}{}", " ".repeat(2_097_153), admission());
    assert!(serde_json::from_str::<AdmissionResponse>(&body).is_ok());
    let server = ScriptedServer::start(vec![Reply::json(202, &body)])
        .await
        .unwrap();
    assert!(ordinary_client(&server, true)
        .submit(logical())
        .await
        .is_err());
    let compact = serde_json::to_string(&delivery()).unwrap();
    let raw = format!("{{{}{}", " ".repeat(98_304), &compact[1..]);
    let server = ScriptedServer::start(vec![Reply::json(200, &poll_body(Some(&raw), vec![]))])
        .await
        .unwrap();
    let keys = SpyFacility::new(false);
    let c = client(
        &server,
        false,
        Arc::new(FixedSource(Ok(None))),
        pins(true),
        keys.clone(),
    );
    let page = c
        .poll(JsonInteger::new(0).unwrap(), PollWait::new(0).unwrap())
        .await
        .expect("an oversized delivery must not discard a valid page");
    assert!(page.deliveries.is_empty());
    assert!(page.receipts.is_empty());
    assert_eq!(page.next_cursor.get(), 0);
    assert_eq!(keys.opens.load(Ordering::SeqCst), 0);
    let requests = server.requests().await;
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].target, "/node/v1/poll?wait=0&receipts_after=0");
}
#[tokio::test]
async fn status_receipt_uses_same_outbox_and_pin_checks() {
    let mut bad = receipt();
    bad.signature = Base64Bytes::new([0; 64]);
    let server = ScriptedServer::start(vec![
        Reply::json(200, &status_json(Some(&bad))),
        Reply::json(200, &status_json(Some(&receipt()))),
    ])
    .await
    .unwrap();
    let c = ordinary_client(&server, true);
    assert!(matches!(
        c.status(logical()).await.unwrap().receipt,
        Some(ReceiptVerification::Rejected(
            ReceiptRejection::InvalidSignature
        ))
    ));
    assert!(matches!(
        c.status(logical()).await.unwrap().receipt,
        Some(ReceiptVerification::Verified(_))
    ));
}
#[tokio::test]
async fn network_failure_is_transport() {
    let server = ScriptedServer::start(vec![]).await.unwrap();
    let c = ordinary_client(&server, true);
    drop(server);
    tokio::task::yield_now().await;
    assert!(matches!(
        c.submit(logical()).await.unwrap_err().channel_error(),
        Some(ChannelError::Transport(_))
    ));
}

fn poll_body_slices(
    deliveries: &[&str],
    receipts: &[&str],
    cursor: u64,
    server_time: &str,
) -> String {
    format!(
        "{{\"deliveries\":[{}],\"receipts\":[{}],\"receipts_cursor\":{},\"server_time\":{}}}",
        deliveries.join(","),
        receipts.join(","),
        cursor,
        serde_json::to_string(server_time).unwrap()
    )
}

fn receipt_item_json(receipt: &WireReceipt, seq: u64) -> String {
    json!({"seq":seq,"receipt":receipt,"recorded_at":"2026-09-24T00:00:00Z"}).to_string()
}

fn spaced_delivery_json() -> String {
    let raw = serde_json::to_string_pretty(&serde_json::to_value(delivery()).unwrap())
        .unwrap()
        .replace("01920000", "\\u00301920000");
    assert_eq!(serde_json::from_str::<Delivery>(&raw).unwrap(), delivery());
    raw
}

#[tokio::test]
async fn poll_accepts_numeric_offset_server_time_and_normalizes_utc_output() {
    for (input, expected) in [
        ("2026-09-24T00:00:00Z", "2026-09-24T00:00:00Z"),
        ("2026-09-24T05:30:00+05:30", "2026-09-24T00:00:00Z"),
        ("2026-09-23T17:00:00-07:00", "2026-09-24T00:00:00Z"),
        ("2026-09-24T00:00:00.123+00:00", "2026-09-24T00:00:00.123Z"),
    ] {
        let body = poll_body_slices(&[], &[], 23, input);
        let server = ScriptedServer::start(vec![Reply::json(200, &body)])
            .await
            .unwrap();
        let page = ordinary_client(&server, true)
            .poll(JsonInteger::new(7).unwrap(), PollWait::new(0).unwrap())
            .await
            .expect("numeric-offset server time must preserve the successful poll");
        assert_eq!(page.next_cursor.get(), 23);
        assert!(page.deliveries.is_empty());
        assert!(page.receipts.is_empty());
        assert_eq!(
            serde_json::to_value(&page.server_time).unwrap(),
            json!(expected)
        );
        assert_eq!(
            page.server_time.as_utc(),
            UtcTimestamp::parse(expected).unwrap().as_utc()
        );
        let requests = server.requests().await;
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].target, "/node/v1/poll?wait=0&receipts_after=7");
    }
}

#[tokio::test]
async fn poll_malformed_first_receipt_keeps_later_verified_receipt_and_cursor() {
    let expected = receipt();
    let first = receipt_item_json(&expected, 1);
    let later = receipt_item_json(&expected, 2);
    let malformed = r#"{ "seq": 1, "receipt": false, "recorded_at": "2026-09-24T00:00:00Z" }"#;
    let server = ScriptedServer::start(vec![
        Reply::json(
            200,
            &poll_body_slices(&[], &[&first], 1, "2026-09-24T00:00:00Z"),
        ),
        Reply::json(
            200,
            &poll_body_slices(&[], &[malformed, &later], 73, "2026-09-24T00:00:00Z"),
        ),
    ])
    .await
    .unwrap();
    let source = Arc::new(OrderedSource {
        outcomes: std::sync::Mutex::new(vec![Ok(Some(persisted())); 2].into()),
        requested: std::sync::Mutex::new(Vec::new()),
    });
    let keys = SpyFacility::new(true);
    let c = client(&server, true, source.clone(), pins(false), keys.clone());
    let guard = c
        .poll(JsonInteger::new(0).unwrap(), PollWait::new(0).unwrap())
        .await
        .expect("the unmodified receipt page must verify first");
    assert_eq!(guard.receipts.len(), 1);
    assert!(matches!(&guard.receipts[0].verification,
        ReceiptVerification::Verified(verified) if verified.receipt() == &expected));
    let page = c
        .poll(guard.next_cursor, PollWait::new(0).unwrap())
        .await
        .expect("a malformed receipt must not hide a later valid receipt or cursor");
    assert_eq!(page.next_cursor.get(), 73);
    assert!(page.deliveries.is_empty());
    assert_eq!(page.receipts.len(), 1);
    assert_eq!(page.receipts[0].item.seq.get(), 2);
    assert!(matches!(&page.receipts[0].verification,
        ReceiptVerification::Verified(verified) if verified.receipt() == &expected));
    assert_eq!(*source.requested.lock().unwrap(), vec![logical(); 2]);
    assert!(source.outcomes.lock().unwrap().is_empty());
    assert_eq!(keys.opens.load(Ordering::SeqCst), 0);
    assert_eq!(keys.seals.load(Ordering::SeqCst), 0);
    let requests = server.requests().await;
    assert_eq!(
        requests.len(),
        2,
        "polling never acknowledges malformed items"
    );
    assert!(requests.iter().all(|request| request.method == "GET"));
    assert_eq!(requests[1].target, "/node/v1/poll?wait=0&receipts_after=1");
}

#[tokio::test]
async fn poll_malformed_first_delivery_keeps_later_raw_bytes_and_cursor() {
    let raw = spaced_delivery_json();
    let mut bad = serde_json::to_value(delivery()).unwrap();
    bad["unexpected"] = json!(true);
    let malformed = serde_json::to_string_pretty(&bad).unwrap();
    let server = ScriptedServer::start(vec![
        Reply::json(
            200,
            &poll_body_slices(&[&raw], &[], 5, "2026-09-24T00:00:00Z"),
        ),
        Reply::json(
            200,
            &poll_body_slices(&[&malformed, &raw], &[], 41, "2026-09-24T00:00:00Z"),
        ),
    ])
    .await
    .unwrap();
    let keys = SpyFacility::new(false);
    let c = client(
        &server,
        false,
        Arc::new(FixedSource(Ok(None))),
        pins(true),
        keys.clone(),
    );
    let guard = c
        .poll(JsonInteger::new(0).unwrap(), PollWait::new(0).unwrap())
        .await
        .expect("the unmodified delivery page must open first");
    assert_eq!(guard.deliveries.len(), 1);
    assert!(matches!(
        guard.deliveries[0].opening(),
        DeliveryOpenResult::Opened(_)
    ));
    assert_eq!(guard.deliveries[0].original_json(), raw.as_bytes());
    assert_eq!(
        guard.deliveries[0].receipt_binding().unwrap(),
        receipt().binding
    );
    assert_eq!(keys.opens.load(Ordering::SeqCst), 1);
    let page = c
        .poll(guard.next_cursor, PollWait::new(0).unwrap())
        .await
        .expect("a malformed delivery must not hide a later valid delivery or cursor");
    assert_eq!(page.next_cursor.get(), 41);
    assert!(page.receipts.is_empty());
    assert_eq!(page.deliveries.len(), 1);
    let valid = &page.deliveries[0];
    assert_eq!(valid.delivery(), &delivery());
    assert!(matches!(valid.opening(), DeliveryOpenResult::Opened(_)));
    assert_eq!(valid.original_json(), raw.as_bytes());
    assert_ne!(
        valid.original_json(),
        serde_json::to_vec(valid.delivery()).unwrap()
    );
    assert_eq!(valid.receipt_binding().unwrap(), receipt().binding);
    assert_eq!(
        keys.opens.load(Ordering::SeqCst),
        2,
        "only the two valid deliveries may open"
    );
    assert_eq!(keys.seals.load(Ordering::SeqCst), 0);
    let requests = server.requests().await;
    assert_eq!(
        requests.len(),
        2,
        "polling never acknowledges malformed items"
    );
    assert!(requests.iter().all(|request| request.method == "GET"));
    assert_eq!(requests[1].target, "/node/v1/poll?wait=0&receipts_after=5");
}

#[tokio::test]
async fn poll_page_fields_and_cardinality_stay_strict_before_any_item_action() {
    let raw = serde_json::to_string(&delivery()).unwrap();
    let receipt_raw = receipt_item_json(&receipt(), 1);
    let valid = poll_body_slices(&[&raw], &[&receipt_raw], 25, "2026-09-24T00:00:00Z");
    let base: Value = serde_json::from_str(&valid).unwrap();
    let mut cases = Vec::new();
    let mut unknown = base.clone();
    unknown["future_page_field"] = json!(true);
    cases.push(("unknown page field", unknown.to_string()));
    for field in ["deliveries", "receipts", "receipts_cursor", "server_time"] {
        let mut missing = base.clone();
        missing.as_object_mut().unwrap().remove(field);
        cases.push((field, missing.to_string()));
    }
    for (field, value) in [
        ("deliveries", json!({})),
        ("receipts", json!(null)),
        ("receipts_cursor", json!("25")),
        ("receipts_cursor", json!(-1)),
        ("server_time", json!("2026-09-24 00:00:00Z")),
    ] {
        let mut invalid = base.clone();
        invalid[field] = value;
        cases.push((field, invalid.to_string()));
    }
    cases.push(("non-object page", "[]".to_owned()));
    cases.push((
        "duplicate page member",
        format!("{{\"deliveries\":[],\"deliveries\":[{raw}],\"receipts\":[{receipt_raw}],\"receipts_cursor\":25,\"server_time\":\"2026-09-24T00:00:00Z\"}}"),
    ));
    let mut too_many_deliveries = base.clone();
    let mut delivery_items = vec![json!(null); 17];
    delivery_items[0] = serde_json::to_value(delivery()).unwrap();
    too_many_deliveries["deliveries"] = Value::Array(delivery_items);
    cases.push(("17 total delivery slots", too_many_deliveries.to_string()));
    let mut too_many_receipts = base.clone();
    let mut receipt_items = vec![json!(null); 65];
    receipt_items[0] = serde_json::from_str(&receipt_raw).unwrap();
    too_many_receipts["receipts"] = Value::Array(receipt_items);
    cases.push(("65 total receipt slots", too_many_receipts.to_string()));
    let mut replies = vec![Reply::json(200, &valid)];
    replies.extend(cases.iter().map(|(_, body)| Reply::json(200, body)));
    let server = ScriptedServer::start(replies).await.unwrap();
    let source = Arc::new(OrderedSource {
        outcomes: std::sync::Mutex::new(vec![Ok(Some(persisted())); cases.len() + 1].into()),
        requested: std::sync::Mutex::new(Vec::new()),
    });
    let keys = SpyFacility::new(false);
    let c = client(&server, false, source.clone(), pins(true), keys.clone());
    let guard = c
        .poll(JsonInteger::new(0).unwrap(), PollWait::new(0).unwrap())
        .await
        .expect("a valid page must exercise both item routes before refusals");
    assert_eq!(guard.next_cursor.get(), 25);
    assert_eq!(guard.deliveries.len(), 1);
    assert_eq!(guard.receipts.len(), 1);
    assert!(matches!(
        guard.deliveries[0].opening(),
        DeliveryOpenResult::Opened(_)
    ));
    assert_eq!(keys.opens.load(Ordering::SeqCst), 1);
    assert_eq!(*source.requested.lock().unwrap(), vec![logical()]);
    for (reason, _) in &cases {
        let error = c
            .poll(JsonInteger::new(25).unwrap(), PollWait::new(0).unwrap())
            .await
            .expect_err("an invalid page must fail before processing any item");
        assert!(
            matches!(error.channel_error(), Some(ChannelError::Transport(_))),
            "{reason}"
        );
        assert_eq!(keys.opens.load(Ordering::SeqCst), 1, "{reason}");
        assert_eq!(
            *source.requested.lock().unwrap(),
            vec![logical()],
            "{reason}"
        );
    }
    assert_eq!(keys.seals.load(Ordering::SeqCst), 0);
    let requests = server.requests().await;
    assert_eq!(requests.len(), cases.len() + 1);
    assert!(requests.iter().all(|request| request.method == "GET"));
}

#[tokio::test]
async fn poll_version_and_ciphertext_refusals_keep_later_delivery_and_cursor() {
    let raw = spaced_delivery_json();
    let mut unsupported = serde_json::to_value(delivery()).unwrap();
    unsupported["protocol_version"] = json!(2);
    let unsupported_raw = serde_json::to_string_pretty(&unsupported).unwrap();
    let mut oversized = serde_json::to_value(delivery()).unwrap();
    oversized["ciphertext"] = json!(encode_base64url(&vec![0; MAX_CIPHERTEXT_BYTES + 1]));
    let oversized_raw = serde_json::to_string_pretty(&oversized).unwrap();
    assert!(Delivery::parse(unsupported_raw.as_bytes()).is_err());
    assert!(Delivery::parse(oversized_raw.as_bytes()).is_err());
    assert!(oversized_raw.len() < crate::request::MAX_REQUEST_BODY_BYTES);
    let server = ScriptedServer::start(vec![
        Reply::json(
            200,
            &poll_body_slices(&[&raw], &[], 5, "2026-09-24T00:00:00Z"),
        ),
        Reply::json(
            200,
            &poll_body_slices(&[&unsupported_raw, &raw], &[], 41, "2026-09-24T00:00:00Z"),
        ),
        Reply::json(
            200,
            &poll_body_slices(&[&oversized_raw, &raw], &[], 42, "2026-09-24T00:00:00Z"),
        ),
    ])
    .await
    .unwrap();
    let keys = SpyFacility::new(false);
    let c = client(
        &server,
        false,
        Arc::new(FixedSource(Ok(None))),
        pins(true),
        keys.clone(),
    );
    let guard = c
        .poll(JsonInteger::new(0).unwrap(), PollWait::new(0).unwrap())
        .await
        .expect("the unmodified delivery page must open first");
    assert_eq!(guard.deliveries.len(), 1);
    assert!(matches!(
        guard.deliveries[0].opening(),
        DeliveryOpenResult::Opened(_)
    ));
    assert_eq!(keys.opens.load(Ordering::SeqCst), 1);
    let mut cursor = guard.next_cursor;
    for (expected_cursor, expected_opens) in [(41, 2), (42, 3)] {
        let page = c
            .poll(cursor, PollWait::new(0).unwrap())
            .await
            .expect("version and ciphertext refusals must preserve the later valid delivery");
        assert_eq!(page.next_cursor.get(), expected_cursor);
        assert!(page.receipts.is_empty());
        assert_eq!(page.deliveries.len(), 1);
        assert_eq!(page.deliveries[0].delivery(), &delivery());
        assert!(matches!(
            page.deliveries[0].opening(),
            DeliveryOpenResult::Opened(_)
        ));
        assert_eq!(page.deliveries[0].original_json(), raw.as_bytes());
        assert_eq!(
            page.deliveries[0].receipt_binding().unwrap(),
            receipt().binding
        );
        assert_eq!(keys.opens.load(Ordering::SeqCst), expected_opens);
        cursor = page.next_cursor;
    }
    assert_eq!(keys.seals.load(Ordering::SeqCst), 0);
    let requests = server.requests().await;
    assert_eq!(
        requests.len(),
        3,
        "neither refused item may cause an acknowledgement"
    );
    assert!(requests.iter().all(|request| request.method == "GET"));
    assert_eq!(requests[1].target, "/node/v1/poll?wait=0&receipts_after=5");
    assert_eq!(requests[2].target, "/node/v1/poll?wait=0&receipts_after=41");
}

#[path = "client_r2_rejection_tests.rs"]
mod r2_rejection_tests;

#[path = "client_r3_regression_tests.rs"]
mod r3_regression_tests;

use crate::encoding::{JsonInteger, PollWait};
use crate::envelope::{EnvelopeHeader, SealedEnvelope};
use crate::keys::KeyFacility;
use crate::pins::{PinIdentity, PinSource, PinState};
use crate::plaintext::classify_plaintext;
use crate::receipt::WireReceipt;
use crate::request::{sign_request, SystemClock, MAX_REQUEST_BODY_BYTES};
use crate::response::*;
use crate::source::{NodeClientBinding, OutboundSource, PersistedSubmission};
use crate::submit::serialize_submission;
use crate::wire::{
    AcknowledgeResponse, AdmissionResponse, BoundedList, ContactResponse, Delivery, MessageState,
    ReceiptItem, RefusalCode, RefusalResponse, ServerTimestamp, StatusResponse,
};
use khive_channel::{
    ChannelError, HoldReason, PendingDetail, ReceiptDisposition, SendOutcome,
    VerifiedRecipientReceipt,
};
use reqwest::{Client, Method, Url};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::value::RawValue;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};
use tokio::sync::Mutex;
use uuid::Uuid;

const MAX_RESPONSE_BYTES: usize = 2_097_152;
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

struct ClientState {
    binding: NodeClientBinding,
    base: Url,
    http: Client,
    source: Arc<dyn OutboundSource>,
    pins: Arc<dyn PinSource>,
    facility: Arc<dyn KeyFacility>,
    poll: Mutex<()>,
    #[cfg(test)]
    auth: std::sync::Mutex<std::collections::VecDeque<(u64, [u8; 16])>>,
}

#[derive(Clone)]
pub struct NodeClient {
    state: Arc<ClientState>,
}

struct HttpResponse {
    status: u16,
    body: Vec<u8>,
    diagnostic: RemoteDiagnostic,
}

// Keep item bytes until each item's own decoder runs. Page members remain
// strict and use the wire model's scalar and bounded-list wrappers.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPollPage {
    deliveries: BoundedList<Box<RawValue>, 16>,
    receipts: BoundedList<Box<RawValue>, 64>,
    receipts_cursor: JsonInteger,
    server_time: ServerTimestamp,
}

impl NodeClient {
    pub fn new(
        binding: NodeClientBinding,
        service_url: &str,
        source: Arc<dyn OutboundSource>,
        pins: Arc<dyn PinSource>,
        facility: Arc<dyn KeyFacility>,
    ) -> Result<Self, NodeError> {
        Self::build(binding, service_url, source, pins, facility, false)
    }
    fn build(
        binding: NodeClientBinding,
        service_url: &str,
        source: Arc<dyn OutboundSource>,
        pins: Arc<dyn PinSource>,
        facility: Arc<dyn KeyFacility>,
        loopback_test: bool,
    ) -> Result<Self, NodeError> {
        let base =
            Url::parse(service_url).map_err(|_| NodeError::invalid("invalid service origin"))?;
        let allowed_scheme = base.scheme() == "https"
            || (cfg!(test)
                && loopback_test
                && base.scheme() == "http"
                && base.host_str() == Some("127.0.0.1"));
        if !allowed_scheme
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.fragment().is_some()
            || base.query().is_some()
            || base.path() != "/"
        {
            return Err(NodeError::invalid(
                "service must be an HTTPS origin without credentials",
            ));
        }
        if binding.namespace.is_empty()
            || binding.slug.is_empty()
            || binding.key_reference.is_empty()
        {
            return Err(NodeError::invalid("missing local node binding"));
        }
        let builder = Client::builder()
            .use_rustls_tls()
            .https_only(!loopback_test)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(Duration::from_secs(10));
        let builder = if loopback_test {
            builder.no_proxy()
        } else {
            builder
        };
        let http = builder
            .build()
            .map_err(|_| NodeError::transport("HTTPS client construction failed"))?;
        Ok(Self {
            state: Arc::new(ClientState {
                binding,
                base,
                http,
                source,
                pins,
                facility,
                poll: Mutex::new(()),
                #[cfg(test)]
                auth: std::sync::Mutex::new(std::collections::VecDeque::new()),
            }),
        })
    }
    #[cfg(test)]
    pub(crate) fn test_http(
        binding: NodeClientBinding,
        service_url: &str,
        source: Arc<dyn OutboundSource>,
        pins: Arc<dyn PinSource>,
        facility: Arc<dyn KeyFacility>,
    ) -> Result<Self, NodeError> {
        Self::build(binding, service_url, source, pins, facility, true)
    }
    #[cfg(test)]
    pub(crate) fn test_auth(&self, values: Vec<(u64, [u8; 16])>) {
        *self.state.auth.lock().unwrap() = values.into();
    }

    async fn request(
        &self,
        method: Method,
        target: &str,
        body: Vec<u8>,
    ) -> Result<HttpResponse, NodeError> {
        if body.len() > MAX_REQUEST_BODY_BYTES {
            return Err(NodeError::invalid("request body exceeds protocol limit"));
        }
        let url = self
            .state
            .base
            .join(target)
            .map_err(|_| NodeError::invalid("invalid node request target"))?;
        let mut request = self
            .state
            .http
            .request(method, url)
            .header("Content-Type", "application/json")
            .body(body.clone())
            .build()
            .map_err(|_| NodeError::transport("request construction failed"))?;
        let url = request.url();
        let target = match url.query() {
            Some(q) => format!("{}?{}", url.path(), q),
            None => url.path().to_owned(),
        };
        let headers = {
            #[cfg(test)]
            let fixed = self.state.auth.lock().unwrap().pop_front();
            #[cfg(not(test))]
            let fixed: Option<(u64, [u8; 16])> = None;
            match fixed {
                Some((timestamp, nonce)) => crate::request::RequestHeaders::sign(
                    self.state.facility.as_ref(),
                    self.state.binding.device,
                    timestamp,
                    nonce,
                    request.method().as_str(),
                    &target,
                    &body,
                ),
                None => sign_request(
                    self.state.facility.as_ref(),
                    &SystemClock,
                    self.state.binding.device,
                    request.method().as_str(),
                    &target,
                    &body,
                ),
            }
            .map_err(|_| NodeError::transport("request authentication could not be created"))?
        };
        for (name, value) in headers.fields() {
            request.headers_mut().insert(
                reqwest::header::HeaderName::from_static(match name {
                    "Khive-Device" => "khive-device",
                    "Khive-Timestamp" => "khive-timestamp",
                    "Khive-Nonce" => "khive-nonce",
                    _ => "khive-signature",
                }),
                value
                    .parse()
                    .map_err(|_| NodeError::invalid("invalid authentication header"))?,
            );
        }
        if request
            .headers()
            .contains_key(reqwest::header::AUTHORIZATION)
        {
            return Err(NodeError::invalid(
                "node requests cannot carry Authorization",
            ));
        }
        let mut response = self
            .state
            .http
            .execute(request)
            .await
            .map_err(|_| NodeError::transport("node network request failed"))?;
        let status = response.status().as_u16();
        let date = response
            .headers()
            .get(reqwest::header::DATE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let clock = (status == 401).then(|| clock_diagnosis(headers.timestamp, date.as_deref()));
        let diagnostic = RemoteDiagnostic {
            status,
            date,
            clock,
            retry_after_seconds: None,
        };
        // Authentication and server failures keep their A.9 class even if their bodies are unusable.
        if status == 401 || status >= 500 {
            let body = read_bounded(&mut response).await.unwrap_or_default();
            return Ok(HttpResponse {
                status,
                body,
                diagnostic,
            });
        }
        let body = read_bounded(&mut response).await?;
        Ok(HttpResponse {
            status,
            body,
            diagnostic,
        })
    }

    pub async fn contact(&self, agent: Uuid) -> Result<NodeContactResult, NodeError> {
        let r = self
            .request(
                Method::GET,
                &format!("/node/v1/contacts/{agent}"),
                Vec::new(),
            )
            .await?;
        if r.status != 200 {
            return Err(refusal_error(&r));
        }
        let observed: ContactResponse = decode(&r.body)?;
        observed
            .validated_keys(&self.state.binding.realm)
            .map_err(|_| NodeError::transport("contact response is inconsistent"))?;
        if observed.agent_id.into_uuid() != agent {
            return Err(NodeError::transport("contact response names another agent"));
        }
        let identity = PinIdentity {
            realm: self.state.binding.realm.clone(),
            agent: observed.agent_id,
            device: observed.device_id,
            epoch: observed.key_epoch,
        };
        let matches_owner_pin = matches!(self.state.pins.resolve(&identity).await?, PinState::Confirmed(ref p) if p.identity() == &identity && p.fingerprint() == &observed.fingerprint);
        Ok(NodeContactResult {
            observed,
            matches_owner_pin,
        })
    }

    pub async fn submit(&self, logical_id: Uuid) -> Result<SendOutcome, NodeError> {
        let Some(p) = self.state.source.current(logical_id).await? else {
            return Err(NodeError::invalid("submission has no persisted envelope"));
        };
        if !p.matches(logical_id, &self.state.binding) {
            return Err(NodeError::invalid(
                "persisted submission does not match node binding",
            ));
        }
        let body = serialize_submission(&p)
            .map_err(|_| NodeError::invalid("persisted submission could not be serialized"))?;
        let r = self
            .request(Method::POST, "/node/v1/messages", body)
            .await?;
        if r.status == 202 || r.status == 200 {
            if let Ok(admission) = serde_json::from_slice::<AdmissionResponse>(&r.body) {
                return Ok(SendOutcome::Pending(PendingDetail::Admitted {
                    admitted_at: admission.admitted_at.as_utc(),
                }));
            }
            if r.status == 202 {
                return Err(NodeError::transport("invalid admission response"));
            }
            let status: StatusResponse = decode(&r.body)?;
            if status.logical_message_id.into_uuid() != logical_id {
                return Ok(unverified(ReceiptRejection::BindingMismatch));
            }
            let Some(receipt) = status.receipt else {
                return Err(NodeError::transport(
                    "submit success contains neither admission nor receipt",
                ));
            };
            return Ok(match self.verify_sender_receipt(&receipt, Some(&p)).await {
                ReceiptVerification::Verified(v) => match v.receipt.disposition {
                    ReceiptDisposition::Stored => SendOutcome::RecipientStored(v.into_verified()),
                    ReceiptDisposition::Quarantined => {
                        SendOutcome::RecipientQuarantined(v.into_verified())
                    }
                },
                ReceiptVerification::Rejected(reason) => unverified(reason),
            });
        }
        let refusal = serde_json::from_slice::<RefusalResponse>(&r.body).ok();
        if r.status != 401 && r.status < 500 {
            match refusal.as_ref().map(|x| x.error) {
                Some(RefusalCode::InsufficientCredit) => {
                    return Ok(SendOutcome::Pending(PendingDetail::Held(
                        HoldReason::InsufficientCredit,
                    )))
                }
                Some(RefusalCode::RecipientKeyChanged) => {
                    return Ok(SendOutcome::Pending(PendingDetail::Held(
                        HoldReason::RecipientKeyChanged,
                    )))
                }
                _ => {}
            }
        }
        Err(refusal_error(&r))
    }

    async fn verify_sender_receipt(
        &self,
        receipt: &WireReceipt,
        expected: Option<&PersistedSubmission>,
    ) -> ReceiptVerification {
        let requested = expected
            .map(|p| p.logical_message_id.into_uuid())
            .unwrap_or_else(|| receipt.binding.logical_message_id.into_uuid());
        let fetched;
        let p = match expected {
            Some(p) => p,
            None => {
                fetched = match self
                    .state
                    .source
                    .current(receipt.binding.logical_message_id.into_uuid())
                    .await
                {
                    Ok(Some(p)) => p,
                    Ok(None) => return rejected(ReceiptRejection::SourceMissing),
                    Err(_) => return rejected(ReceiptRejection::SourceUnavailable),
                };
                &fetched
            }
        };
        if !p.matches(requested, &self.state.binding) {
            return rejected(ReceiptRejection::SourceMismatch);
        }
        let b = &receipt.binding;
        if b.protocol_version != p.protocol_version
            || b.logical_message_id != p.logical_message_id
            || b.sender_agent_id != p.sender_agent
            || b.recipient_agent_id != p.recipient_agent
            || b.recipient_device_id != p.recipient_device
            || b.recipient_key_epoch != p.recipient_key_epoch
            || b.contact_generation != p.contact_generation
        {
            return rejected(ReceiptRejection::BindingMismatch);
        }
        let identity = PinIdentity {
            realm: self.state.binding.realm.clone(),
            agent: p.recipient_agent,
            device: p.recipient_device,
            epoch: p.recipient_key_epoch,
        };
        let pin = match self.state.pins.resolve(&identity).await {
            Ok(PinState::Confirmed(pin)) => pin,
            Ok(PinState::FingerprintMismatch) => {
                return rejected(ReceiptRejection::FingerprintMismatch)
            }
            Ok(_) => return rejected(ReceiptRejection::PinUnconfirmed),
            Err(_) => return rejected(ReceiptRejection::PinUnavailable),
        };
        if pin.identity() != &identity || pin.fingerprint() != &p.recipient_fingerprint {
            return rejected(ReceiptRejection::FingerprintMismatch);
        }
        let verified = match VerifiedRecipientReceipt::verify(
            receipt.to_channel(),
            pin.keys().signing.as_bytes(),
        ) {
            Ok(verified) => verified,
            Err(_) => return rejected(ReceiptRejection::InvalidSignature),
        };
        ReceiptVerification::Verified(VerifiedSenderReceipt {
            receipt: receipt.clone(),
            verified,
        })
    }

    pub async fn poll(
        &self,
        cursor: JsonInteger,
        wait: PollWait,
    ) -> Result<NodePollResult, NodeError> {
        let _guard = self.state.poll.lock().await;
        let target = format!(
            "/node/v1/poll?wait={}&receipts_after={}",
            wait.get(),
            cursor.get()
        );
        let r = self.request(Method::GET, &target, Vec::new()).await?;
        if r.status != 200 {
            return Err(refusal_error(&r));
        }
        let page: RawPollPage = decode(&r.body)?;
        let mut deliveries = Vec::new();
        let mut rejected_deliveries = Vec::new();
        for (index, raw) in page.deliveries.into_vec().into_iter().enumerate() {
            let original = raw.get().as_bytes().to_vec();
            if original.len() > MAX_REQUEST_BODY_BYTES {
                rejected_deliveries.push(NodePollRejection {
                    index,
                    code: RefusalCode::PayloadTooLarge,
                    original,
                });
                continue;
            }
            // The explicit entry point preserves typed version/size refusals
            // without decoding any neighbouring item first.
            let delivery = match Delivery::parse(&original) {
                Ok(delivery) => delivery,
                Err(error) => {
                    rejected_deliveries.push(NodePollRejection {
                        index,
                        code: error.refusal_code(),
                        original,
                    });
                    continue;
                }
            };
            let opening = self.open_delivery(&delivery).await;
            deliveries.push(NodeDelivery {
                index,
                delivery,
                original,
                opening,
            });
        }
        let mut receipts = Vec::new();
        let mut rejected_receipts = Vec::new();
        for (index, raw) in page.receipts.into_vec().into_iter().enumerate() {
            let receipt = match serde_json::from_str::<ReceiptItem>(raw.get()) {
                Ok(receipt) => receipt,
                Err(_) => {
                    rejected_receipts.push(NodePollRejection {
                        index,
                        code: RefusalCode::InvalidRequest,
                        original: raw.get().as_bytes().to_vec(),
                    });
                    continue;
                }
            };
            let verification = self.verify_sender_receipt(&receipt.receipt, None).await;
            receipts.push(NodeReceiptResult {
                index,
                item: receipt,
                verification,
            });
        }
        Ok(NodePollResult {
            deliveries,
            receipts,
            rejected_deliveries,
            rejected_receipts,
            next_cursor: page.receipts_cursor,
            server_time: page.server_time,
        })
    }

    async fn open_delivery(&self, d: &Delivery) -> DeliveryOpenResult {
        let local = &self.state.binding;
        if d.recipient_agent_id != local.agent
            || d.recipient_device_id != local.device
            || d.recipient_key_epoch != local.key_epoch
        {
            return held(HeldReason::WrongRecipient);
        }
        let identity = PinIdentity {
            realm: local.realm.clone(),
            agent: d.sender_agent_id,
            device: d.sender_device_id,
            epoch: d.sender_key_epoch,
        };
        let pin = match self.state.pins.resolve(&identity).await {
            Ok(PinState::Confirmed(pin)) => pin,
            Ok(PinState::NonContact) => return held(HeldReason::NonContact),
            Ok(PinState::UnconfirmedEpoch { .. }) => {
                return held(HeldReason::UnconfirmedSenderEpoch)
            }
            Ok(PinState::FingerprintMismatch) => return held(HeldReason::FingerprintMismatch),
            Err(_) => return held(HeldReason::PinUnavailable),
        };
        if pin.identity() != &identity {
            return held(HeldReason::FingerprintMismatch);
        }
        let header = EnvelopeHeader {
            protocol_version: d.protocol_version,
            realm: local.realm.clone(),
            sender_agent_id: d.sender_agent_id,
            sender_device_id: d.sender_device_id,
            sender_key_epoch: d.sender_key_epoch,
            recipient_agent_id: d.recipient_agent_id,
            recipient_device_id: d.recipient_device_id,
            recipient_key_epoch: d.recipient_key_epoch,
        };
        let envelope = SealedEnvelope {
            enc: d.enc.clone(),
            ciphertext: d.ciphertext.clone(),
        };
        match self
            .state
            .facility
            .open(&header, d.logical_message_id, &pin.keys().kem, &envelope)
        {
            Ok(plaintext) => DeliveryOpenResult::Opened(classify_plaintext(&plaintext)),
            Err(_) => held(HeldReason::EnvelopeDidNotAuthenticate),
        }
    }

    /// The caller supplies only a post-commit journal receipt; this call writes no local state.
    pub async fn ack(&self, receipt: &WireReceipt) -> Result<(), NodeError> {
        let b = &receipt.binding;
        let local = &self.state.binding;
        if b.recipient_agent_id != local.agent
            || b.recipient_device_id != local.device
            || b.recipient_key_epoch != local.key_epoch
            || receipt
                .verify(&self.state.facility.public_keys().signing)
                .is_err()
        {
            return Err(NodeError::invalid(
                "acknowledgement is not this device's signed receipt",
            ));
        }
        let body = serde_json::to_vec(receipt)
            .map_err(|_| NodeError::invalid("invalid acknowledgement"))?;
        let r = self
            .request(Method::POST, "/node/v1/receipts", body)
            .await?;
        if r.status == 200 {
            let _: AcknowledgeResponse = decode(&r.body)?;
            return Ok(());
        }
        if r.status != 401
            && r.status < 500
            && matches!(
                serde_json::from_slice::<RefusalResponse>(&r.body),
                Ok(RefusalResponse {
                    error: RefusalCode::NotFound,
                    ..
                })
            )
        {
            return Ok(());
        }
        Err(refusal_error(&r))
    }

    pub async fn status(&self, logical_id: Uuid) -> Result<NodeStatusResult, NodeError> {
        let r = self
            .request(
                Method::GET,
                &format!("/node/v1/messages/{logical_id}"),
                Vec::new(),
            )
            .await?;
        if r.status != 200 {
            return Err(refusal_error(&r));
        }
        let status: StatusResponse = decode(&r.body)?;
        if status.logical_message_id.into_uuid() != logical_id {
            return Err(NodeError::transport("status names another logical message"));
        }
        let receipt = match status.receipt {
            Some(ref receipt) => Some(
                if receipt.binding.logical_message_id.into_uuid() != logical_id {
                    rejected(ReceiptRejection::BindingMismatch)
                } else {
                    self.verify_sender_receipt(receipt, None).await
                },
            ),
            None => None,
        };
        if matches!(
            status.state,
            MessageState::RecipientStored | MessageState::RecipientQuarantined
        ) && receipt.is_none()
        {
            return Err(NodeError::transport("final status has no receipt"));
        }
        Ok(NodeStatusResult {
            logical_message_id: status.logical_message_id,
            state: status.state,
            receipt,
        })
    }
}

fn unverified(reason: ReceiptRejection) -> SendOutcome {
    SendOutcome::Pending(PendingDetail::ReceiptUnverified {
        reason: format!("{reason:?}"),
    })
}
fn rejected(reason: ReceiptRejection) -> ReceiptVerification {
    ReceiptVerification::Rejected(reason)
}
fn held(reason: HeldReason) -> DeliveryOpenResult {
    DeliveryOpenResult::HeldUnopened(reason)
}
fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T, NodeError> {
    serde_json::from_slice(body).map_err(|_| NodeError::transport("invalid node response JSON"))
}

async fn read_bounded(response: &mut reqwest::Response) -> Result<Vec<u8>, NodeError> {
    if response
        .content_length()
        .is_some_and(|n| n > MAX_RESPONSE_BYTES as u64)
    {
        return Err(NodeError::transport("node response exceeds local limit"));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| NodeError::transport("node response read failed"))?
    {
        if body
            .len()
            .checked_add(chunk.len())
            .is_none_or(|n| n > MAX_RESPONSE_BYTES)
        {
            return Err(NodeError::transport("node response exceeds local limit"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn clock_diagnosis(timestamp: u64, date: Option<&str>) -> ClockDiagnosis {
    let server = date
        .and_then(|d| httpdate::parse_http_date(d).ok())
        .and_then(|d| d.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs());
    match server {
        None => ClockDiagnosis::DateUnavailable,
        Some(server) if i128::from(timestamp) - i128::from(server) >= 60 => {
            ClockDiagnosis::AtLeast60SecondsAhead
        }
        Some(server) if i128::from(server) - i128::from(timestamp) > 300 => {
            ClockDiagnosis::MoreThan300SecondsBehind
        }
        Some(_) => ClockDiagnosis::WithinSkewBounds,
    }
}

fn refusal_error(r: &HttpResponse) -> NodeError {
    let refusal = serde_json::from_slice::<RefusalResponse>(&r.body).ok();
    let mut diagnostic = r.diagnostic.clone();
    diagnostic.retry_after_seconds = refusal
        .as_ref()
        .and_then(|x| x.retry_after_seconds.map(|n| n.get()));
    let code = refusal.as_ref().map(|x| x.error);
    let error = if r.status == 401 {
        ChannelError::Auth("node authentication refused".into())
    } else if r.status >= 500
        || matches!(
            code,
            Some(
                RefusalCode::RecipientOffline
                    | RefusalCode::PollInProgress
                    | RefusalCode::RateLimited
                    | RefusalCode::CapacityExhausted
            )
        )
    {
        ChannelError::Transport(format!("node refusal {code:?}"))
    } else if refusal.is_none() {
        if (400..500).contains(&r.status) && !matches!(r.status, 408 | 429) {
            ChannelError::PermanentTransport("invalid node refusal response".into())
        } else {
            ChannelError::Transport("invalid node refusal response".into())
        }
    } else {
        ChannelError::PermanentTransport(format!("node refusal {code:?}"))
    };
    NodeError::Channel {
        error,
        diagnostic: Some(diagnostic),
    }
}

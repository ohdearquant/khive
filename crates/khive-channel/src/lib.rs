//! Channel transport abstraction (ADR-056).
//!
//! This crate defines the `Channel` trait, `ChannelEnvelope`, `ChannelRegistry`, and
//! `ChannelError`. Concrete transport adapters (e.g. `khive-channel-email`) implement
//! the `Channel` trait; the MCP server polls registered channels and ingests inbound
//! messages via the `comm.ingest` subhandler verb.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// In-memory replay context retained only until an inbound envelope is
/// durably handled.
///
/// The exact transport bytes deliberately bypass `metadata`: metadata is sent
/// through `comm.ingest` and would re-present content to the same write gate
/// that can reject the message. Serialization skips this context, and its
/// `Debug` implementation reports only byte length so logs cannot expose the
/// original message.
#[derive(Clone)]
pub struct QuarantineReplay {
    /// Byte-exact transport payload stored in content-addressed blob storage.
    pub bytes: Vec<u8>,
    /// Channel address that should receive a body-free quarantine notification.
    pub notification_to: String,
}

impl std::fmt::Debug for QuarantineReplay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuarantineReplay")
            .field("bytes_len", &self.bytes.len())
            .field("notification_to", &self.notification_to)
            .finish()
    }
}

/// A message envelope passed between the channel transport and the runtime.
///
/// Outbound envelopes are produced by the runtime and delivered by a `Channel`.
/// Inbound envelopes are produced by a `Channel` and consumed by the polling loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelEnvelope {
    /// Sender address in `channel-kind:address` form, e.g. `email:alice@example.com`.
    pub from: String,
    /// Recipient address in `channel-kind:address` form.
    pub to: String,
    /// Message body (plain text).
    pub content: String,
    /// Optional subject line (used by email and similar channels).
    pub subject: Option<String>,
    /// RFC 3339 timestamp of when the message was originally sent or received.
    pub sent_at: Option<DateTime<Utc>>,
    /// External deduplication key.
    ///
    /// For IMAP email the format is `imap:{host}:{uidvalidity}:{uid}` (e.g.
    /// `imap:mail.example.com:1234567:42`).  This key is derived from the IMAP
    /// UIDVALIDITY and UID values, not from the RFC 822 `Message-ID` header.
    /// Adapters must not populate this field when UIDVALIDITY or UID is absent
    /// or zero; `comm.ingest` performs atomic dedup against the unique index on
    /// this field.
    pub external_id: Option<String>,
    /// External correlation key used to resolve the thread (e.g. X-Khive-Thread-ID header
    /// or In-Reply-To header value for email). The handler resolves this to an internal UUID.
    pub correlation_external_id: Option<String>,
    /// Arbitrary transport-specific key-value metadata.
    pub metadata: HashMap<String, String>,
    /// Non-serialized replay context for a message that may need quarantine.
    #[serde(skip)]
    pub quarantine_replay: Option<QuarantineReplay>,
    /// RFC 822 Message-ID to set on the outbound email (including angle brackets,
    /// e.g. `<uuid@domain>`). `None` on inbound envelopes and when the transport
    /// should auto-generate the identifier.
    pub message_id: Option<String>,
    /// This email's own RFC 822 `Message-ID` header value, as received (including
    /// angle brackets). `None` on outbound envelopes and when the inbound message
    /// carried no `Message-ID`. Distinct from `external_id`, which is the IMAP
    /// UIDVALIDITY/UID dedup key, not a wire Message-ID.
    pub wire_message_id: Option<String>,
    /// This email's own RFC 822 `References` header value, as received verbatim
    /// (space-separated angle-bracketed ids). `None` on outbound envelopes and
    /// when the inbound message carried no `References` header. Captured so a
    /// reply can extend the ancestor chain rather than truncating it to just the
    /// immediate parent (issue #403).
    pub wire_references: Option<String>,
    /// RFC 822 `In-Reply-To` value to set on an outbound reply (including angle
    /// brackets, e.g. `<uuid@domain>`). `None` when the reply has no known
    /// parent Message-ID, or on inbound envelopes.
    pub in_reply_to: Option<String>,
    /// RFC 822 `References` value to set on an outbound reply: the parent's
    /// existing References chain (if any) followed by the parent's Message-ID,
    /// space-separated angle-bracketed ids. `None` on inbound envelopes. When
    /// the reply has a known parent Message-ID but no chain to extend, this is
    /// `None` and the SMTP layer falls back to `in_reply_to` alone.
    pub references: Option<String>,
}

impl ChannelEnvelope {
    /// Create a minimal outbound envelope.
    pub fn new(from: impl Into<String>, to: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            from: from.into(),
            to: to.into(),
            content: content.into(),
            subject: None,
            sent_at: None,
            external_id: None,
            correlation_external_id: None,
            metadata: HashMap::new(),
            quarantine_replay: None,
            message_id: None,
            wire_message_id: None,
            wire_references: None,
            in_reply_to: None,
            references: None,
        }
    }

    /// Attach a subject line.
    pub fn with_subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }

    /// Attach a sent-at timestamp.
    pub fn with_sent_at(mut self, ts: DateTime<Utc>) -> Self {
        self.sent_at = Some(ts);
        self
    }

    /// Attach an external deduplication key.
    pub fn with_external_id(mut self, id: impl Into<String>) -> Self {
        self.external_id = Some(id.into());
        self
    }

    /// Retain exact source bytes and the safe notification target in memory.
    pub fn with_quarantine_replay(
        mut self,
        bytes: Vec<u8>,
        notification_to: impl Into<String>,
    ) -> Self {
        self.quarantine_replay = Some(QuarantineReplay {
            bytes,
            notification_to: notification_to.into(),
        });
        self
    }

    /// Attach a correlation key for thread resolution.
    pub fn with_correlation(mut self, correlation: impl Into<String>) -> Self {
        self.correlation_external_id = Some(correlation.into());
        self
    }

    /// Attach an RFC 822 Message-ID (including angle brackets) to set on the outbound email.
    pub fn with_message_id(mut self, id: impl Into<String>) -> Self {
        self.message_id = Some(id.into());
        self
    }

    /// Attach this inbound email's own RFC 822 Message-ID (including angle brackets).
    pub fn with_wire_message_id(mut self, id: impl Into<String>) -> Self {
        self.wire_message_id = Some(id.into());
        self
    }

    /// Attach this inbound email's own RFC 822 References chain, verbatim.
    pub fn with_wire_references(mut self, references: impl Into<String>) -> Self {
        self.wire_references = Some(references.into());
        self
    }

    /// Attach the parent Message-ID (including angle brackets) this outbound reply
    /// should set as `In-Reply-To`.
    pub fn with_in_reply_to(mut self, id: impl Into<String>) -> Self {
        self.in_reply_to = Some(id.into());
        self
    }

    /// Attach the full References chain (parent's existing chain, if any, followed
    /// by the parent's Message-ID) this outbound reply should set as `References`.
    pub fn with_references(mut self, references: impl Into<String>) -> Self {
        self.references = Some(references.into());
        self
    }
}

/// Durable poll progress for a single `(kind, slug)` channel.
///
/// Transport-neutral: IMAP maps `generation` to `UIDVALIDITY` and
/// `high_water` to the greatest durably handled UID, but the type itself
/// carries no IMAP-specific meaning so other transports can reuse it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelCheckpoint {
    /// Stable, non-secret identity of the remote source/configuration.
    pub source: String,
    /// Remote identity epoch. IMAP maps this to UIDVALIDITY.
    pub generation: u64,
    /// Greatest durably handled remote sequence value. IMAP maps this to UID.
    pub high_water: Option<u64>,
}

/// A [`ChannelCheckpoint`] as persisted, with the time it was committed.
///
/// `committed_at` is used as the recovery `SINCE` floor after an epoch reset,
/// so a daemon that was down across a UIDVALIDITY change still bounds its
/// re-scan instead of falling back to the caller's `since` alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredChannelCheckpoint {
    #[serde(flatten)]
    pub checkpoint: ChannelCheckpoint,
    /// Database commit time, used as the recovery SINCE floor after an epoch reset.
    pub committed_at: DateTime<Utc>,
}

/// The result of one [`Channel::poll_page`] call: envelopes ready for
/// `comm.ingest`, plus the checkpoint the poll coordinator should persist
/// after every envelope has been durably ingested.
#[derive(Debug, Clone)]
pub struct ChannelPollPage {
    pub envelopes: Vec<ChannelEnvelope>,
    /// `Some` only when durable progress must be inserted or changed.
    pub next_checkpoint: Option<ChannelCheckpoint>,
}

impl ChannelPollPage {
    /// Wrap a plain envelope list with no checkpoint — the default
    /// [`Channel::poll_page`] behavior for adapters that only implement `poll`.
    pub fn stateless(envelopes: Vec<ChannelEnvelope>) -> Self {
        Self {
            envelopes,
            next_checkpoint: None,
        }
    }
}

/// Transport identity covered by a recipient's signature (ADR-105).
///
/// Agent identifiers are immutable service identities, distinct from local actor
/// labels. They come from authenticated routing authority, never asserted envelope
/// fields. This is a value to sign or verify, not proof of authenticity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryReceiptBinding {
    pub protocol_version: u32,
    pub logical_message_id: Uuid,
    pub sender_agent_id: String,
    pub recipient_agent_id: String,
    pub recipient_device_id: Uuid,
    pub recipient_key_epoch: u64,
    pub contact_generation: u64,
    pub delivery_attempt_id: Uuid,
}

/// The recipient's durable ingest outcome; this says nothing about read state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptDisposition {
    Stored,
    Quarantined,
}

/// A signed recipient commit receipt, containing no message content or local note id.
///
/// The signature covers both `binding` and `disposition`. Signature bytes are
/// opaque here: the node protocol defines their encoding, the canonical signed
/// bytes, and verification against the pinned recipient key. Deserializing this
/// type does not verify its signature or authorize a delivery-state transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryReceipt {
    pub binding: DeliveryReceiptBinding,
    pub disposition: ReceiptDisposition,
    pub signature: Vec<u8>,
}

/// Result of a receipt-aware outbound submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendOutcome {
    /// The legacy adapter accepted the send; not a recipient commit receipt.
    LegacyAccepted,
    /// The sender retains the message until a verified recipient receipt arrives.
    Pending(PendingDetail),
    RecipientStored(DeliveryReceipt),
    RecipientQuarantined(DeliveryReceipt),
}

/// Admission or hold information retained while waiting for a verified recipient receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingDetail {
    /// The service admitted the submission (`202`, or `200` with state `pending`).
    /// The runtime uses this timestamp to schedule resubmission if no receipt arrives.
    Admitted { admitted_at: DateTime<Utc> },
    /// The service refused with an outcome that holds the message until the owner acts.
    Held(HoldReason),
    /// A receipt failed sender verification; the message stays pending with retry backoff.
    ReceiptUnverified { reason: String },
}

/// A service refusal that suspends retry until the owner acts, leaving the message pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldReason {
    /// The sender's account cannot pay for admission (`402 insufficient_credit`).
    InsufficientCredit,
    /// The recipient's device or key epoch changed (`409 recipient_key_changed`).
    /// Retry waits for the owner's confirmation of the new fingerprint.
    RecipientKeyChanged,
}

impl SendOutcome {
    /// Reject an outcome whose variant contradicts its receipt disposition.
    ///
    /// The node adapter must additionally authenticate the receipt, check its
    /// binding against the pending submission, and refuse `LegacyAccepted` as
    /// proof of delivery. This check performs none of those protocol operations.
    pub fn validate_receipt(&self) -> Result<(), ChannelError> {
        match self {
            Self::RecipientStored(receipt) if receipt.disposition != ReceiptDisposition::Stored => {
                Err(ChannelError::InvalidEnvelope(
                    "recipient_stored outcome requires a stored receipt".into(),
                ))
            }
            Self::RecipientQuarantined(receipt)
                if receipt.disposition != ReceiptDisposition::Quarantined =>
            {
                Err(ChannelError::InvalidEnvelope(
                    "recipient_quarantined outcome requires a quarantined receipt".into(),
                ))
            }
            _ => Ok(()),
        }
    }
}

/// In-memory receipt authority accompanying one authenticated inbound envelope.
///
/// Tickets are moved with their envelope, never cloned or deserialized from wire
/// input. The node adapter supplies authenticated routing identity; the runtime
/// must reject a mismatched or duplicated ticket before verified-recipient ingest.
/// Non-clonability alone does not authenticate a ticket or prevent protocol replay.
///
/// ```compile_fail
/// use khive_channel::InboundReceiptTicket;
/// fn duplicate(ticket: InboundReceiptTicket) {
///     let _duplicate = ticket.clone();
/// }
/// ```
#[derive(Debug)]
pub struct InboundReceiptTicket {
    binding: DeliveryReceiptBinding,
    sender_key_epoch: u64,
}

impl InboundReceiptTicket {
    /// Create a ticket at the trusted adapter boundary after authenticating the
    /// envelope and its routing identity. This constructor performs no verification.
    pub fn new(binding: DeliveryReceiptBinding, sender_key_epoch: u64) -> Self {
        Self {
            binding,
            sender_key_epoch,
        }
    }

    pub fn binding(&self) -> &DeliveryReceiptBinding {
        &self.binding
    }

    pub fn sender_key_epoch(&self) -> u64 {
        self.sender_key_epoch
    }
}

/// A poll page with one receipt-ticket slot per envelope, in the same order.
///
/// Private fields preserve the checked count for the lifetime of the page.
/// Construction checks cardinality only; the runtime still validates the ticket
/// binding and rejects duplicates. Polling does not acknowledge a delivery.
#[derive(Debug)]
pub struct DeliveryPage {
    page: ChannelPollPage,
    tickets: Vec<Option<InboundReceiptTicket>>,
}

impl DeliveryPage {
    pub fn new(
        page: ChannelPollPage,
        tickets: Vec<Option<InboundReceiptTicket>>,
    ) -> Result<Self, ChannelError> {
        if page.envelopes.len() != tickets.len() {
            return Err(ChannelError::InvalidEnvelope(format!(
                "delivery page has {} envelopes but {} receipt ticket slots",
                page.envelopes.len(),
                tickets.len(),
            )));
        }
        Ok(Self { page, tickets })
    }

    /// Preserve a legacy page, including its checkpoint, with no receipt tickets.
    pub fn legacy(page: ChannelPollPage) -> Self {
        let tickets = (0..page.envelopes.len()).map(|_| None).collect();
        Self { page, tickets }
    }

    pub fn page(&self) -> &ChannelPollPage {
        &self.page
    }

    pub fn tickets(&self) -> &[Option<InboundReceiptTicket>] {
        &self.tickets
    }

    /// Consume the page to transfer its envelopes and their aligned ticket slots.
    /// The consumer owns preserving that pairing and validating binding/replay
    /// before ingest; the page can no longer enforce alignment after consumption.
    pub fn into_parts(self) -> (ChannelPollPage, Vec<Option<InboundReceiptTicket>>) {
        (self.page, self.tickets)
    }
}

/// Errors produced by channel operations.
#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    /// Configuration is missing or invalid.
    #[error("channel configuration error: {0}")]
    Config(String),
    /// Transport-level connection or I/O failure.
    #[error("transport error: {0}")]
    Transport(String),
    /// A definitive transport rejection for which retrying the same envelope
    /// cannot help (for example SMTP 5xx or a Telegram client-error response).
    #[error("permanent transport error: {0}")]
    PermanentTransport(String),
    /// Authentication failure (TLS, credentials, etc.).
    #[error("authentication error: {0}")]
    Auth(String),
    /// A minted credential was refused and can be refreshed in process.
    #[error("refreshable authentication error: {0}")]
    RetryableAuth(String),
    /// Message was rejected because the sender is not authorized.
    #[error("unauthorized sender: {0}")]
    UnauthorizedSender(String),
    /// The envelope is malformed or missing required fields.
    #[error("invalid envelope: {0}")]
    InvalidEnvelope(String),
}

/// Whether an outbound delivery error should remain pending or terminate the
/// message. Attempt count never changes this classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryFailureClass {
    Transient,
    Permanent,
}

impl ChannelError {
    /// Classify this error for durable outbound-note delivery state.
    pub fn delivery_failure_class(&self) -> DeliveryFailureClass {
        match self {
            Self::Transport(_) | Self::RetryableAuth(_) => DeliveryFailureClass::Transient,
            Self::Config(_)
            | Self::PermanentTransport(_)
            | Self::Auth(_)
            | Self::UnauthorizedSender(_)
            | Self::InvalidEnvelope(_) => DeliveryFailureClass::Permanent,
        }
    }
}

/// A channel transport adapter.
///
/// Implementors handle outbound delivery (`send`) and inbound polling (`poll`).
/// Each adapter is identified by a stable kind string (e.g. `"email"`).
///
/// Note: `Debug` is intentionally NOT required. Concrete adapters hold credentials;
/// requiring `Debug` would risk password leakage in logs via derived impls.
#[async_trait]
pub trait Channel: Send + Sync + 'static {
    /// Short stable identifier for this transport (e.g. `"email"`, `"telegram"`).
    fn kind(&self) -> &'static str;

    /// Stable per-credential identifier distinguishing multiple configured
    /// accounts of the same `kind` (e.g. two mailboxes both polled via the
    /// `"email"` channel). khive #606: channel health rows are keyed by
    /// `(kind, slug)`, never `kind` alone, so two accounts of the same kind
    /// never collapse into a single heartbeat row.
    ///
    /// The default implementation returns `kind()` — correct for any
    /// transport that only ever has a single configured credential per
    /// process. Adapters that can have distinct per-credential identity
    /// (e.g. email's mailbox address) should override this.
    fn slug(&self) -> String {
        self.kind().to_string()
    }

    /// Return `true` when this adapter has sufficient configuration to operate.
    ///
    /// The default implementation returns `true`; adapters with optional config
    /// may override this to report their readiness without returning errors from
    /// `poll` or `send` on every call.
    fn is_configured(&self) -> bool {
        true
    }

    /// Send a single outbound message.
    ///
    /// Outbound write-back (reply routing from the KG note layer) is deferred
    /// to a future release; `send` exists so the trait surface is complete.
    async fn send(&self, envelope: ChannelEnvelope) -> Result<(), ChannelError>;

    /// Submit an outbound message and report recipient-commit progress (ADR-105).
    ///
    /// The default delegates to [`Channel::send`] unchanged and returns
    /// [`SendOutcome::LegacyAccepted`]. The node adapter overrides this to return
    /// `Pending`, `RecipientStored`, or `RecipientQuarantined`, and never treats
    /// legacy acceptance as proof of delivery.
    async fn send_with_receipt(
        &self,
        envelope: ChannelEnvelope,
    ) -> Result<SendOutcome, ChannelError> {
        self.send(envelope).await?;
        Ok(SendOutcome::LegacyAccepted)
    }

    /// Poll for new inbound messages since `since`.
    ///
    /// Returns envelopes ready to be forwarded to `comm.ingest`.  Deduplication
    /// is performed by `comm.ingest` via `INSERT OR IGNORE` against the
    /// `idx_comm_message_external_id` unique index; adapters do not need to
    /// deduplicate themselves.  Adapters should apply a best-effort server-side
    /// filter on `since` to avoid fetching large backlogs.
    async fn poll(&self, since: DateTime<Utc>) -> Result<Vec<ChannelEnvelope>, ChannelError>;

    /// Poll for new inbound messages, checkpoint-aware.
    ///
    /// The default implementation wraps [`Channel::poll`] with a stateless
    /// [`ChannelPollPage`] (`next_checkpoint: None`), so every existing
    /// adapter remains source-compatible without change. Adapters that can
    /// bind progress to a durable per-source high-water mark (e.g. IMAP's
    /// UIDVALIDITY/UID) should override this instead of relying on `since`
    /// alone.
    async fn poll_page(
        &self,
        since: DateTime<Utc>,
        checkpoint: Option<&StoredChannelCheckpoint>,
    ) -> Result<ChannelPollPage, ChannelError> {
        let _ = checkpoint;
        Ok(ChannelPollPage::stateless(self.poll(since).await?))
    }

    /// Poll envelopes with receipt tickets while preserving checkpoint semantics.
    ///
    /// The default wraps [`Channel::poll_page`] with all-`None` ticket slots.
    /// The node adapter overrides this to attach authenticated, envelope-bound
    /// tickets. Polling itself never acknowledges or deletes a delivery.
    async fn poll_deliveries(
        &self,
        since: DateTime<Utc>,
        checkpoint: Option<&StoredChannelCheckpoint>,
    ) -> Result<DeliveryPage, ChannelError> {
        Ok(DeliveryPage::legacy(
            self.poll_page(since, checkpoint).await?,
        ))
    }

    /// Forward a signed receipt after durable recipient ingest has committed.
    ///
    /// The default returns a permanent unsupported configuration error. The node
    /// adapter must override this; the runtime drives it from a durable
    /// acknowledgement journal so failed acknowledgements retry after restart.
    async fn acknowledge_receipt(&self, _receipt: &DeliveryReceipt) -> Result<(), ChannelError> {
        Err(ChannelError::Config(format!(
            "receipts unsupported by {}",
            self.kind()
        )))
    }
}

/// Registry of named channel adapters.
///
/// The MCP server holds an `Arc<ChannelRegistry>` and polls all registered
/// channels in a background loop, ingesting results via `comm.ingest`.
///
/// Keyed by the composite `(kind, slug)` identity (khive #606), NOT `kind`
/// alone: two accounts of the same `kind`
/// (e.g. two mailboxes, both `kind() == "email"`) must coexist as distinct
/// registered adapters, or they collapse before the heartbeat writer ever
/// gets a chance to persist separate `channel_health` rows for them.
#[derive(Default)]
pub struct ChannelRegistry {
    channels: HashMap<(String, String), Arc<dyn Channel>>,
}

impl ChannelRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a channel adapter. Replaces any previous adapter with the
    /// same `(kind, slug)` composite identity — the pre-#606 "same-kind
    /// replaces" semantics still hold for the common single-credential-per-
    /// kind case (where `slug()` falls back to `kind()`), but two adapters
    /// of the same kind with distinct `slug()` values now coexist.
    pub fn register(&mut self, channel: Arc<dyn Channel>) {
        let key = (channel.kind().to_string(), channel.slug());
        self.channels.insert(key, channel);
    }

    /// Look up a channel by kind only.
    ///
    /// When multiple adapters share `kind` (distinct `slug()` values), this
    /// returns an unspecified one of them (`HashMap` iteration order is not
    /// defined) — it exists for the common single-credential-per-kind case.
    /// Callers that must resolve a specific credential among several sharing
    /// a kind need [`ChannelRegistry::get_by_slug`]. No production call site
    /// resolves outbound `send` through this registry today (the outbox loop
    /// holds its own `Arc<EmailChannel>` directly), so no send-path
    /// resolution currently depends on which adapter this returns when
    /// several share a kind.
    pub fn get(&self, kind: &str) -> Option<Arc<dyn Channel>> {
        self.channels
            .iter()
            .find(|((k, _), _)| k == kind)
            .map(|(_, v)| Arc::clone(v))
    }

    /// Look up a channel by its exact `(kind, slug)` composite identity.
    pub fn get_by_slug(&self, kind: &str, slug: &str) -> Option<Arc<dyn Channel>> {
        self.channels
            .get(&(kind.to_string(), slug.to_string()))
            .cloned()
    }

    /// Iterate over all registered channels as `(kind, slug, channel)` triples.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str, &Arc<dyn Channel>)> {
        self.channels
            .iter()
            .map(|((k, s), v)| (k.as_str(), s.as_str(), v))
    }

    /// Returns `true` if no channels are registered.
    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
    }

    /// Number of registered channels.
    pub fn len(&self) -> usize {
        self.channels.len()
    }
}

/// Generate a new correlation ID suitable for embedding in a message header.
pub fn new_thread_correlation_id() -> String {
    Uuid::new_v4().as_hyphenated().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct LegacyChannel {
        sent: Mutex<Vec<ChannelEnvelope>>,
        inbound: Vec<ChannelEnvelope>,
        fail: bool,
    }

    #[async_trait]
    impl Channel for LegacyChannel {
        fn kind(&self) -> &'static str {
            "legacy"
        }

        async fn send(&self, envelope: ChannelEnvelope) -> Result<(), ChannelError> {
            if self.fail {
                return Err(ChannelError::Transport("send unavailable".into()));
            }
            self.sent.lock().unwrap().push(envelope);
            Ok(())
        }

        async fn poll(&self, _since: DateTime<Utc>) -> Result<Vec<ChannelEnvelope>, ChannelError> {
            if self.fail {
                return Err(ChannelError::UnauthorizedSender("poll denied".into()));
            }
            Ok(self.inbound.clone())
        }
    }

    fn receipt(disposition: ReceiptDisposition) -> DeliveryReceipt {
        DeliveryReceipt {
            binding: DeliveryReceiptBinding {
                protocol_version: 1,
                logical_message_id: Uuid::new_v4(),
                sender_agent_id: "sender-agent".into(),
                recipient_agent_id: "recipient-agent".into(),
                recipient_device_id: Uuid::new_v4(),
                recipient_key_epoch: 2,
                contact_generation: 3,
                delivery_attempt_id: Uuid::new_v4(),
            },
            disposition,
            signature: vec![1, 2, 3],
        }
    }

    #[tokio::test]
    async fn receipt_defaults_preserve_legacy_send_and_poll() {
        let envelope = ChannelEnvelope::new("legacy:sender", "legacy:recipient", "body")
            .with_subject("subject")
            .with_sent_at(Utc::now())
            .with_external_id("external-id")
            .with_correlation("thread-id")
            .with_quarantine_replay(vec![0, 255, 13, 10], "legacy:sender");
        let expected_bytes = serde_json::to_vec(&envelope).unwrap();
        let adapter = LegacyChannel {
            sent: Mutex::new(Vec::new()),
            inbound: vec![envelope.clone()],
            fail: false,
        };
        let channel: &dyn Channel = &adapter;
        assert_eq!(
            channel.send_with_receipt(envelope).await.unwrap(),
            SendOutcome::LegacyAccepted
        );
        {
            let sent = adapter.sent.lock().unwrap();
            assert_eq!(sent.len(), 1);
            assert_eq!(serde_json::to_vec(&sent[0]).unwrap(), expected_bytes);
            assert_eq!(
                sent[0].quarantine_replay.as_ref().unwrap().bytes,
                [0, 255, 13, 10]
            );
        }
        let deliveries = channel.poll_deliveries(Utc::now(), None).await.unwrap();
        assert_eq!(deliveries.tickets().len(), 1);
        assert!(deliveries.tickets().iter().all(Option::is_none));
        let (page, tickets) = deliveries.into_parts();
        assert_eq!(page.envelopes.len(), tickets.len());
        assert_eq!(
            serde_json::to_vec(&page.envelopes[0]).unwrap(),
            expected_bytes
        );
        assert_eq!(
            page.envelopes[0].quarantine_replay.as_ref().unwrap().bytes,
            [0, 255, 13, 10]
        );
        assert!(page.next_checkpoint.is_none());

        let error = channel
            .acknowledge_receipt(&receipt(ReceiptDisposition::Stored))
            .await
            .unwrap_err();
        assert_eq!(
            error.delivery_failure_class(),
            DeliveryFailureClass::Permanent
        );
        assert!(
            matches!(error, ChannelError::Config(message) if message == "receipts unsupported by legacy")
        );
    }

    #[tokio::test]
    async fn receipt_defaults_propagate_legacy_errors() {
        let channel = LegacyChannel {
            sent: Mutex::new(Vec::new()),
            inbound: vec![],
            fail: true,
        };
        assert!(matches!(
            channel.send_with_receipt(ChannelEnvelope::new("a", "b", "c")).await,
            Err(ChannelError::Transport(message)) if message == "send unavailable"
        ));
        assert!(matches!(
            channel.poll_deliveries(Utc::now(), None).await,
            Err(ChannelError::UnauthorizedSender(message)) if message == "poll denied"
        ));
        assert!(channel.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn poll_deliveries_preserves_overridden_page_and_checkpoint() {
        struct CheckpointChannel {
            since: DateTime<Utc>,
            checkpoint: StoredChannelCheckpoint,
        }

        #[async_trait]
        impl Channel for CheckpointChannel {
            fn kind(&self) -> &'static str {
                "checkpoint"
            }

            async fn send(&self, _: ChannelEnvelope) -> Result<(), ChannelError> {
                panic!("poll_deliveries must not send");
            }

            async fn poll(&self, _: DateTime<Utc>) -> Result<Vec<ChannelEnvelope>, ChannelError> {
                panic!("poll_deliveries must use the poll_page override");
            }

            async fn poll_page(
                &self,
                since: DateTime<Utc>,
                checkpoint: Option<&StoredChannelCheckpoint>,
            ) -> Result<ChannelPollPage, ChannelError> {
                assert_eq!(since, self.since);
                assert_eq!(checkpoint, Some(&self.checkpoint));
                Ok(ChannelPollPage {
                    envelopes: vec![ChannelEnvelope::new("a", "b", "checkpointed")],
                    next_checkpoint: Some(ChannelCheckpoint {
                        high_water: Some(43),
                        ..self.checkpoint.checkpoint.clone()
                    }),
                })
            }
        }

        let channel = CheckpointChannel {
            since: Utc::now(),
            checkpoint: StoredChannelCheckpoint {
                checkpoint: ChannelCheckpoint {
                    source: "source".into(),
                    generation: 7,
                    high_water: Some(42),
                },
                committed_at: Utc::now(),
            },
        };
        let page = channel
            .poll_deliveries(channel.since, Some(&channel.checkpoint))
            .await
            .unwrap();
        assert_eq!(page.page().envelopes[0].content, "checkpointed");
        assert_eq!(
            page.page().next_checkpoint,
            Some(ChannelCheckpoint {
                high_water: Some(43),
                ..channel.checkpoint.checkpoint.clone()
            })
        );
        assert!(page.tickets()[0].is_none());
    }

    #[test]
    fn delivery_page_checks_counts_and_retains_ticket_order() {
        let make_page = || {
            ChannelPollPage::stateless(vec![
                ChannelEnvelope::new("a", "b", "first"),
                ChannelEnvelope::new("a", "b", "second"),
            ])
        };
        for count in [0, 1, 3] {
            let tickets = (0..count).map(|_| None).collect();
            let error = DeliveryPage::new(make_page(), tickets).unwrap_err();
            assert!(matches!(error, ChannelError::InvalidEnvelope(_)));
            assert_eq!(
                error.delivery_failure_class(),
                DeliveryFailureClass::Permanent
            );
        }
        let binding = receipt(ReceiptDisposition::Stored).binding;
        let ticket = InboundReceiptTicket::new(binding.clone(), 9);
        let page = DeliveryPage::new(make_page(), vec![None, Some(ticket)]).unwrap();
        assert_eq!(page.page().envelopes.len(), 2);
        assert!(page.tickets()[0].is_none());
        let ticket = page.tickets()[1].as_ref().unwrap();
        assert_eq!(ticket.binding(), &binding);
        assert_eq!(ticket.sender_key_epoch(), 9);
        assert!(DeliveryPage::new(ChannelPollPage::stateless(vec![]), vec![]).is_ok());
    }

    #[test]
    fn receipt_disposition_is_closed_and_outcomes_must_agree() {
        let stored = receipt(ReceiptDisposition::Stored);
        let quarantined = receipt(ReceiptDisposition::Quarantined);
        assert!(SendOutcome::LegacyAccepted.validate_receipt().is_ok());
        assert!(SendOutcome::Pending(PendingDetail::Admitted {
            admitted_at: Utc::now(),
        })
        .validate_receipt()
        .is_ok());
        assert!(
            SendOutcome::Pending(PendingDetail::Held(HoldReason::InsufficientCredit))
                .validate_receipt()
                .is_ok()
        );
        assert!(
            SendOutcome::Pending(PendingDetail::Held(HoldReason::RecipientKeyChanged))
                .validate_receipt()
                .is_ok()
        );
        assert!(SendOutcome::Pending(PendingDetail::ReceiptUnverified {
            reason: "receipt signature does not match the pinned key".into(),
        })
        .validate_receipt()
        .is_ok());
        assert!(SendOutcome::RecipientStored(stored.clone())
            .validate_receipt()
            .is_ok());
        assert!(SendOutcome::RecipientQuarantined(quarantined.clone())
            .validate_receipt()
            .is_ok());
        assert!(SendOutcome::RecipientStored(quarantined)
            .validate_receipt()
            .is_err());
        assert!(SendOutcome::RecipientQuarantined(stored.clone())
            .validate_receipt()
            .is_err());
        let mut value = serde_json::to_value(&stored).unwrap();
        assert_eq!(value["disposition"], "stored");
        assert_eq!(
            serde_json::from_value::<DeliveryReceipt>(value.clone()).unwrap(),
            stored
        );
        value["disposition"] = serde_json::json!("pending");
        assert!(serde_json::from_value::<DeliveryReceipt>(value).is_err());
    }

    struct MockChannel {
        sent: Arc<Mutex<Vec<ChannelEnvelope>>>,
        inbound: Vec<ChannelEnvelope>,
        // `None` -> `slug()` falls back to the trait default (`kind()`).
        // `Some(_)` -> distinct per-credential identity, for #606's
        // same-kind-different-slug regressions.
        slug: Option<String>,
    }

    impl MockChannel {
        fn new(inbound: Vec<ChannelEnvelope>) -> Self {
            Self {
                sent: Arc::new(Mutex::new(Vec::new())),
                inbound,
                slug: None,
            }
        }

        fn with_slug(inbound: Vec<ChannelEnvelope>, slug: impl Into<String>) -> Self {
            Self {
                sent: Arc::new(Mutex::new(Vec::new())),
                inbound,
                slug: Some(slug.into()),
            }
        }
    }

    #[async_trait]
    impl Channel for MockChannel {
        fn kind(&self) -> &'static str {
            "mock"
        }

        fn slug(&self) -> String {
            self.slug.clone().unwrap_or_else(|| self.kind().to_string())
        }

        async fn send(&self, envelope: ChannelEnvelope) -> Result<(), ChannelError> {
            self.sent.lock().unwrap().push(envelope);
            Ok(())
        }

        async fn poll(&self, _since: DateTime<Utc>) -> Result<Vec<ChannelEnvelope>, ChannelError> {
            Ok(self.inbound.clone())
        }
    }

    #[test]
    fn envelope_builder_fields() {
        let ts = Utc::now();
        let env = ChannelEnvelope::new("email:a@example.com", "email:b@example.com", "hello")
            .with_subject("Test")
            .with_sent_at(ts)
            .with_external_id("<msg1@example.com>")
            .with_correlation("correlation-uuid")
            .with_message_id("<abc123@example.com>")
            .with_wire_message_id("<wire123@example.com>")
            .with_wire_references("<ref1@example.com> <ref2@example.com>")
            .with_in_reply_to("<parent123@example.com>")
            .with_references("<ref1@example.com> <parent123@example.com>");

        assert_eq!(env.from, "email:a@example.com");
        assert_eq!(env.to, "email:b@example.com");
        assert_eq!(env.content, "hello");
        assert_eq!(env.subject.as_deref(), Some("Test"));
        assert_eq!(env.sent_at, Some(ts));
        assert_eq!(env.external_id.as_deref(), Some("<msg1@example.com>"));
        assert_eq!(
            env.correlation_external_id.as_deref(),
            Some("correlation-uuid")
        );
        assert_eq!(env.message_id.as_deref(), Some("<abc123@example.com>"));
        assert_eq!(
            env.wire_message_id.as_deref(),
            Some("<wire123@example.com>")
        );
        assert_eq!(
            env.wire_references.as_deref(),
            Some("<ref1@example.com> <ref2@example.com>")
        );
        assert_eq!(env.in_reply_to.as_deref(), Some("<parent123@example.com>"));
        assert_eq!(
            env.references.as_deref(),
            Some("<ref1@example.com> <parent123@example.com>")
        );
    }

    #[test]
    fn envelope_new_defaults_wire_message_id_and_in_reply_to_to_none() {
        let env = ChannelEnvelope::new("email:a@example.com", "email:b@example.com", "hello");
        assert_eq!(env.wire_message_id, None);
        assert_eq!(env.wire_references, None);
        assert_eq!(env.in_reply_to, None);
        assert_eq!(env.references, None);
    }

    #[test]
    fn registry_register_and_get() {
        let mut reg = ChannelRegistry::new();
        let ch = Arc::new(MockChannel::new(vec![]));
        reg.register(ch);
        assert!(reg.get("mock").is_some());
        assert!(reg.get("email").is_none());
        assert_eq!(reg.len(), 1);
        assert!(!reg.is_empty());
    }

    #[test]
    fn registry_replaces_existing_same_composite_identity() {
        // Two registrations with the SAME (kind, slug) — both default to
        // slug() == kind() here — still replace in place, matching the
        // pre-#606 single-credential-per-kind semantics.
        let mut reg = ChannelRegistry::new();
        reg.register(Arc::new(MockChannel::new(vec![])));
        reg.register(Arc::new(MockChannel::new(vec![])));
        assert_eq!(reg.len(), 1, "same (kind, slug) replaces");
    }

    #[test]
    fn registry_does_not_collapse_same_kind_distinct_slug() {
        // #606: two accounts sharing
        // `kind()` but with distinct `slug()` values (e.g. two mailboxes)
        // must coexist as two registered adapters, not collapse into one.
        let mut reg = ChannelRegistry::new();
        reg.register(Arc::new(MockChannel::with_slug(vec![], "a@example.com")));
        reg.register(Arc::new(MockChannel::with_slug(vec![], "b@example.com")));
        assert_eq!(
            reg.len(),
            2,
            "same kind + distinct slug must coexist, not collapse"
        );
        assert!(reg.get_by_slug("mock", "a@example.com").is_some());
        assert!(reg.get_by_slug("mock", "b@example.com").is_some());
    }

    #[test]
    fn registry_iter_yields_all() {
        let mut reg = ChannelRegistry::new();
        reg.register(Arc::new(MockChannel::new(vec![])));
        let kinds: Vec<&str> = reg.iter().map(|(k, _, _)| k).collect();
        assert_eq!(kinds, vec!["mock"]);
    }

    #[test]
    fn registry_iter_yields_both_slugs_for_same_kind() {
        let mut reg = ChannelRegistry::new();
        reg.register(Arc::new(MockChannel::with_slug(vec![], "a@example.com")));
        reg.register(Arc::new(MockChannel::with_slug(vec![], "b@example.com")));
        let mut slugs: Vec<&str> = reg.iter().map(|(_, s, _)| s).collect();
        slugs.sort_unstable();
        assert_eq!(slugs, vec!["a@example.com", "b@example.com"]);
    }

    #[test]
    fn channel_error_display() {
        let e = ChannelError::Config("missing host".into());
        assert!(e.to_string().contains("missing host"));
        let e2 = ChannelError::UnauthorizedSender("attacker@example.com".into());
        assert!(e2.to_string().contains("attacker@example.com"));
    }

    #[test]
    fn outbound_delivery_failure_classification_separates_pressure_from_rejection() {
        assert_eq!(
            ChannelError::Transport("connection reset".into()).delivery_failure_class(),
            DeliveryFailureClass::Transient
        );
        assert_eq!(
            ChannelError::RetryableAuth("minted token rejected".into()).delivery_failure_class(),
            DeliveryFailureClass::Transient
        );
        assert_eq!(
            ChannelError::PermanentTransport("550 recipient rejected".into())
                .delivery_failure_class(),
            DeliveryFailureClass::Permanent
        );
        for error in [
            ChannelError::Config("missing host".into()),
            ChannelError::Auth("invalid credentials".into()),
            ChannelError::UnauthorizedSender("denied".into()),
            ChannelError::InvalidEnvelope("bad recipient".into()),
        ] {
            assert_eq!(
                error.delivery_failure_class(),
                DeliveryFailureClass::Permanent,
                "static failure must not remain in a raw-cadence retry loop: {error}"
            );
        }
    }

    #[test]
    fn new_thread_correlation_id_is_uuid() {
        let id = new_thread_correlation_id();
        assert!(
            id.parse::<Uuid>().is_ok(),
            "correlation id must be a valid UUID"
        );
    }

    #[test]
    fn is_configured_default_returns_true() {
        let ch = MockChannel::new(vec![]);
        // Default impl must return true; concrete adapters may override.
        assert!(
            ch.is_configured(),
            "default is_configured() must return true"
        );
    }

    #[tokio::test]
    async fn default_poll_page_wraps_legacy_poll_without_checkpoint() {
        // #449: an adapter that implements only `poll` (the pre-checkpoint
        // shape) must still work through `poll_page`, returning the same
        // envelopes with no checkpoint to persist.
        let inbound =
            vec![
                ChannelEnvelope::new("email:sender@example.com", "email:me@example.com", "body")
                    .with_external_id("<id1@example.com>"),
            ];
        let ch = MockChannel::new(inbound.clone());
        let page = ch.poll_page(Utc::now(), None).await.expect("poll_page ok");
        assert_eq!(page.envelopes.len(), 1);
        assert_eq!(
            page.envelopes[0].external_id.as_deref(),
            Some("<id1@example.com>")
        );
        assert!(
            page.next_checkpoint.is_none(),
            "default poll_page must never produce a checkpoint"
        );
    }

    #[tokio::test]
    async fn mock_channel_send_and_poll() {
        let inbound =
            vec![
                ChannelEnvelope::new("email:sender@example.com", "email:me@example.com", "body")
                    .with_external_id("<id1@example.com>"),
            ];
        let ch = Arc::new(MockChannel::new(inbound.clone()));
        let env_out =
            ChannelEnvelope::new("email:me@example.com", "email:them@example.com", "reply");
        ch.send(env_out).await.expect("send ok");
        assert_eq!(ch.sent.lock().unwrap().len(), 1);

        let polled = ch.poll(Utc::now()).await.expect("poll ok");
        assert_eq!(polled.len(), 1);
        assert_eq!(polled[0].external_id.as_deref(), Some("<id1@example.com>"));
    }
}

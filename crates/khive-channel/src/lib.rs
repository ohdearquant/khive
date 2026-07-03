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

/// Errors produced by channel operations.
#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    /// Configuration is missing or invalid.
    #[error("channel configuration error: {0}")]
    Config(String),
    /// Transport-level connection or I/O failure.
    #[error("transport error: {0}")]
    Transport(String),
    /// Authentication failure (TLS, credentials, etc.).
    #[error("authentication error: {0}")]
    Auth(String),
    /// Message was rejected because the sender is not authorized.
    #[error("unauthorized sender: {0}")]
    UnauthorizedSender(String),
    /// The envelope is malformed or missing required fields.
    #[error("invalid envelope: {0}")]
    InvalidEnvelope(String),
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

    /// Poll for new inbound messages since `since`.
    ///
    /// Returns envelopes ready to be forwarded to `comm.ingest`.  Deduplication
    /// is performed by `comm.ingest` via `INSERT OR IGNORE` against the
    /// `idx_comm_message_external_id` unique index; adapters do not need to
    /// deduplicate themselves.  Adapters should apply a best-effort server-side
    /// filter on `since` to avoid fetching large backlogs.
    async fn poll(&self, since: DateTime<Utc>) -> Result<Vec<ChannelEnvelope>, ChannelError>;
}

/// Registry of named channel adapters.
///
/// The MCP server holds an `Arc<ChannelRegistry>` and polls all registered
/// channels in a background loop, ingesting results via `comm.ingest`.
#[derive(Default)]
pub struct ChannelRegistry {
    channels: HashMap<String, Arc<dyn Channel>>,
}

impl ChannelRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a channel adapter. Replaces any previous adapter with the same `kind`.
    pub fn register(&mut self, channel: Arc<dyn Channel>) {
        self.channels.insert(channel.kind().to_string(), channel);
    }

    /// Look up a channel by kind.
    pub fn get(&self, kind: &str) -> Option<Arc<dyn Channel>> {
        self.channels.get(kind).cloned()
    }

    /// Iterate over all registered channels.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Arc<dyn Channel>)> {
        self.channels.iter().map(|(k, v)| (k.as_str(), v))
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

    struct MockChannel {
        sent: Arc<Mutex<Vec<ChannelEnvelope>>>,
        inbound: Vec<ChannelEnvelope>,
    }

    impl MockChannel {
        fn new(inbound: Vec<ChannelEnvelope>) -> Self {
            Self {
                sent: Arc::new(Mutex::new(Vec::new())),
                inbound,
            }
        }
    }

    #[async_trait]
    impl Channel for MockChannel {
        fn kind(&self) -> &'static str {
            "mock"
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
    fn registry_replaces_existing() {
        let mut reg = ChannelRegistry::new();
        reg.register(Arc::new(MockChannel::new(vec![])));
        reg.register(Arc::new(MockChannel::new(vec![])));
        assert_eq!(reg.len(), 1, "same kind replaces");
    }

    #[test]
    fn registry_iter_yields_all() {
        let mut reg = ChannelRegistry::new();
        reg.register(Arc::new(MockChannel::new(vec![])));
        let kinds: Vec<&str> = reg.iter().map(|(k, _)| k).collect();
        assert_eq!(kinds, vec!["mock"]);
    }

    #[test]
    fn channel_error_display() {
        let e = ChannelError::Config("missing host".into());
        assert!(e.to_string().contains("missing host"));
        let e2 = ChannelError::UnauthorizedSender("attacker@example.com".into());
        assert!(e2.to_string().contains("attacker@example.com"));
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

//! Parameter structs and deserializer for comm pack verb handlers.

use serde::Deserialize;
use serde_json::Value;

use khive_runtime::RuntimeError;

// deny_unknown_fields so typo kwargs are rejected at deserialization rather than silently dropped.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SendParams {
    pub to: String,
    pub content: String,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InboxParams {
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadParams {
    pub id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReplyParams {
    pub id: String,
    pub content: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ThreadParams {
    /// Thread root ID: accepts either an 8-char short prefix or a full UUID.
    /// Returns all messages whose `properties.thread_id` matches this value,
    /// plus the originating message itself, in chronological order.
    pub id: String,
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Parameters for `comm.ingest` — ingests a single inbound message from a channel adapter.
///
/// `deny_unknown_fields` is intentionally absent: the polling loop may pass extra fields
/// (including the `namespace` routing key consumed by the dispatch layer) that future
/// handler versions can extend without breaking existing deployments.
///
/// The `namespace` key is consumed by `VerbRegistry::dispatch` to mint the `NamespaceToken`
/// before the handler is called; the handler uses `token` directly and does not read
/// `namespace` from this struct.
#[derive(Deserialize)]
pub(crate) struct IngestParams {
    /// Sender address in `channel-kind:addr` form.
    pub from: String,
    /// Recipient address in `channel-kind:addr` form.
    pub to: String,
    /// Message body text.
    pub content: String,
    #[serde(default)]
    pub subject: Option<String>,
    /// Internal thread UUID. When absent, a new thread root is created.
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub channel_kind: Option<String>,
    /// Stable transport dedup key. For email: `imap:{host}:{uidvalidity}:{uid}`. Duplicate messages are silently ignored.
    #[serde(default)]
    pub external_id: Option<String>,
    /// RFC 3339 timestamp of the original message.
    #[serde(default)]
    pub sent_at: Option<String>,
    /// External correlation key for thread resolution (e.g. X-Khive-Thread-ID or In-Reply-To value).
    #[serde(default)]
    pub correlation_external_id: Option<String>,
    /// Actor to route to when no correlation resolves a prior sender (e.g. `"lambda:leo"`).
    /// When absent and no correlation match is found, falls back to `p.to.trim()`.
    #[serde(default)]
    pub default_inbound_actor: Option<String>,
    /// This message's own RFC 822 Message-ID (including angle brackets), when the
    /// channel adapter captured one. Distinct from `external_id` (the transport
    /// dedup key). Persisted so a later reply to this note can set In-Reply-To /
    /// References for native MUA conversation grouping.
    #[serde(default)]
    pub wire_message_id: Option<String>,
    /// This message's own RFC 822 References header value, verbatim, when the
    /// channel adapter captured one. Persisted so a later reply to this note can
    /// extend the full ancestor chain instead of truncating it to this message's
    /// own Message-ID (issue #403).
    #[serde(default)]
    pub wire_references: Option<String>,
    /// Optional transport-layer metadata passthrough, merged verbatim into the
    /// stored note's properties alongside the fields above. Generic and
    /// channel-agnostic: the comm pack does not interpret any key in this map,
    /// it only persists it. A channel adapter that needs to attach adapter-specific
    /// markers (e.g. the email channel's quarantine flags, ADR-056 Amendment
    /// 2026-07-02) sets `ChannelEnvelope.metadata`; the MCP poll loop forwards it
    /// here unchanged. Absent metadata is today's behavior exactly (issue #448
    /// Finding 2).
    #[serde(default)]
    pub metadata: Option<serde_json::Map<String, Value>>,
}

/// Parameters for `comm.heartbeat` — persists a per-channel-credential heartbeat row.
///
/// `deny_unknown_fields` is intentionally absent, matching `IngestParams`: the
/// `namespace` routing key is consumed by `VerbRegistry::dispatch` before the
/// handler sees `params`, not read from this struct.
#[derive(Deserialize)]
pub(crate) struct HeartbeatParams {
    pub channel_kind: String,
    pub channel_slug: String,
    pub outcome: String,
    #[serde(default)]
    pub error_class: Option<String>,
    #[serde(default)]
    pub error_message: Option<String>,
    #[serde(default)]
    pub at: Option<String>,
}

/// Parameters for `comm.probe` — read-only poll for new inbound message
/// metadata and stale unread count. Public polling contract (khive #667
/// daemon hardening slice): shape is frozen, see the comm pack README.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProbeParams {
    pub actor: String,
    #[serde(default)]
    pub since_us: Option<i64>,
    #[serde(default = "default_stale_minutes")]
    pub stale_minutes: i64,
}

fn default_stale_minutes() -> i64 {
    20
}

pub(crate) fn deser<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RuntimeError> {
    serde_json::from_value(params)
        .map_err(|e| RuntimeError::InvalidInput(format!("bad params: {e}")))
}

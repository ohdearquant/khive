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
    /// Structured provenance tags, persisted verbatim on both copies (issue #495).
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// Opt-in to send to one's own actor identity (khive #820). See
    /// crates/khive-pack-comm/docs/api/message-lifecycle.md#handlersrshandle_send
    #[serde(default)]
    pub self_send: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InboxParams {
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub status: Option<String>,
    /// Exact match on `properties.from_actor`. Mutually exclusive with `from_prefix`.
    #[serde(default)]
    pub from_actor: Option<String>,
    /// Prefix match on `properties.from_actor` (e.g. `"agent:khive:"` selects all
    /// agents under one namespace). Mutually exclusive with `from_actor`.
    #[serde(default)]
    pub from_prefix: Option<String>,
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
    /// Structured provenance tags, persisted verbatim to `properties["tags"]` on
    /// both the outbound and inbound copies of the reply (issue #495).
    #[serde(default)]
    pub tags: Option<Vec<String>>,
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
    /// `"asc"` (default) | `"desc"`. Truncation to `limit` applies after
    /// ordering (issue #494 — long threads previously lost the tail).
    #[serde(default)]
    pub order: Option<String>,
    /// Message id or RFC 3339 timestamp cursor. See
    /// crates/khive-pack-comm/docs/api/message-lifecycle.md#handlersrshandle_thread
    #[serde(default)]
    pub after: Option<String>,
}

/// Parameters for `comm.ingest` — ingests a single inbound message from a
/// channel adapter. `deny_unknown_fields` is intentionally absent (forward-
/// compat with future handler fields); `namespace` is consumed by
/// `VerbRegistry::dispatch`, not read from this struct. See
/// crates/khive-pack-comm/docs/api/message-lifecycle.md#handlersrshandle_ingest
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
    /// Actor to route to when no correlation resolves a prior sender. Falls
    /// back to `p.to.trim()` when absent and no correlation match is found.
    #[serde(default)]
    pub default_inbound_actor: Option<String>,
    /// This message's own RFC 822 Message-ID (angle brackets included), distinct
    /// from `external_id` (the transport dedup key). Persisted for In-Reply-To/
    /// References on a later reply.
    #[serde(default)]
    pub wire_message_id: Option<String>,
    /// This message's own RFC 822 References header value, verbatim. Persisted
    /// so a later reply can extend the full ancestor chain (issue #403).
    #[serde(default)]
    pub wire_references: Option<String>,
    /// Transport-layer metadata passthrough, merged verbatim into the stored
    /// note's properties. Generic and channel-agnostic — see
    /// docs/api/message-lifecycle.md#handlersrshandle_ingest.
    #[serde(default)]
    pub metadata: Option<serde_json::Map<String, Value>>,
}

/// Parameters for `comm.heartbeat` — persists a per-channel-credential heartbeat row.
/// `deny_unknown_fields` is intentionally absent, matching `IngestParams`.
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

/// Parameters for `comm.cursor_get` — reads the persisted channel poll
/// checkpoint for `(channel_kind, channel_slug)`, or `null` if none exists.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CursorGetParams {
    pub channel_kind: String,
    pub channel_slug: String,
}

/// Parameters for `comm.cursor_commit` — persists a channel poll checkpoint
/// for `(channel_kind, channel_slug)`, replacing any prior row for that
/// identity. Only the daemon's channel poll loop calls this, after every
/// envelope in the page has been durably accepted by `comm.ingest`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CursorCommitParams {
    pub channel_kind: String,
    pub channel_slug: String,
    pub source: String,
    pub generation: u64,
    #[serde(default)]
    pub high_water: Option<u64>,
}

pub(crate) fn deser<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RuntimeError> {
    serde_json::from_value(params)
        .map_err(|e| RuntimeError::InvalidInput(format!("bad params: {e}")))
}

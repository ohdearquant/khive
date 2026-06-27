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
}

pub(crate) fn deser<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RuntimeError> {
    serde_json::from_value(params)
        .map_err(|e| RuntimeError::InvalidInput(format!("bad params: {e}")))
}

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

pub(crate) fn deser<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, RuntimeError> {
    serde_json::from_value(params)
        .map_err(|e| RuntimeError::InvalidInput(format!("bad params: {e}")))
}

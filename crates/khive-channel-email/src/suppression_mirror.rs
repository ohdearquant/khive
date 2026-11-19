//! Pull client for the remote suppression mirror (ADR-200 §6).
//!
//! The one-click unsubscribe endpoint is a public service that cannot reach the daemon, so the
//! daemon pulls. One read returns the complete set of suppressed recipient keys with the time
//! the remote read it. This module only reads and checks that answer; recording the keys in the
//! local suppression list is the caller's.

use std::time::Duration;

use serde::Deserialize;

/// The largest response body read. At about 70 bytes per recipient key this holds roughly
/// 450,000 keys; a larger answer fails the refresh with this limit named, never a truncated set.
pub const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

/// One deadline covering connection, headers and the whole body.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// One successful read of the remote set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSuppressionSet {
    /// The remote's own time for the read, RFC 3339.
    pub as_of: String,
    /// Every suppressed recipient key, 64 lowercase hex characters each.
    pub recipient_keys: Vec<String>,
}

/// The answer's wire form. Unknown fields are ignored, so an incremental form can be added later
/// without breaking this reader.
#[derive(Deserialize)]
struct WireSet {
    as_of: String,
    recipient_keys: Vec<String>,
    #[serde(default)]
    count: Option<u64>,
}

/// Read the complete remote set from `url` with `credential` as a bearer token.
///
/// Redirects are not followed, so the credential is only ever sent to the configured URL. Any
/// answer other than `200` with a well-formed body is an error: an `as_of` that is not RFC 3339,
/// a `count` that differs from the number of keys, or a key that is not 64 lowercase hex
/// characters fails the whole read, because a partly trusted set could release a message to an
/// address that opted out. No error carries the credential.
pub async fn fetch_remote_suppression_set(
    url: &str,
    credential: &str,
) -> Result<RemoteSuppressionSet, String> {
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("suppression mirror client could not be built: {error}"))?;
    let mut response = client
        .get(url)
        .bearer_auth(credential)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|error| format!("suppression mirror request failed: {}", error.without_url()))?;
    let status = response.status();
    if status != reqwest::StatusCode::OK {
        return Err(format!(
            "suppression mirror answered HTTP {}",
            status.as_u16()
        ));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(format!(
            "suppression mirror answer is larger than the {MAX_RESPONSE_BYTES}-byte limit"
        ));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        format!(
            "suppression mirror answer could not be read: {}",
            error.without_url()
        )
    })? {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(format!(
                "suppression mirror answer is larger than the {MAX_RESPONSE_BYTES}-byte limit"
            ));
        }
        body.extend_from_slice(&chunk);
    }
    parse_remote_suppression_set(&body)
}

/// Check one answer body. Split from the request so every refusal is testable without a server.
pub(crate) fn parse_remote_suppression_set(body: &[u8]) -> Result<RemoteSuppressionSet, String> {
    let wire: WireSet = serde_json::from_slice(body)
        .map_err(|error| format!("suppression mirror answer is not the expected JSON: {error}"))?;
    if chrono::DateTime::parse_from_rfc3339(&wire.as_of).is_err() {
        return Err("suppression mirror answer has an as_of that is not RFC 3339".into());
    }
    if let Some(count) = wire.count {
        if count != wire.recipient_keys.len() as u64 {
            return Err(format!(
                "suppression mirror answer counts {count} keys but carries {}",
                wire.recipient_keys.len()
            ));
        }
    }
    if let Some(position) = wire
        .recipient_keys
        .iter()
        .position(|key| key.len() != 64 || !khive_types::is_lowercase_hex(key))
    {
        return Err(format!(
            "suppression mirror key at position {position} is not 64 lowercase hex characters"
        ));
    }
    Ok(RemoteSuppressionSet {
        as_of: wire.as_of,
        recipient_keys: wire.recipient_keys,
    })
}

#[cfg(test)]
#[path = "suppression_mirror_tests.rs"]
mod tests;

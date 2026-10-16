//! Telegram Bot API connector: a thin, injectable seam over `sendMessage` /
//! `getUpdates` so `TelegramChannel` can be unit-tested against a mock
//! implementation (no live network calls in tests), mirroring the
//! `SmtpConnector`/`ImapConnector` split in `khive-channel-email`.
//!
//! Plain HTTPS + JSON only — no Telegram SDK (ADR-056 Amendment 2026-07-05).

use async_trait::async_trait;
use khive_channel::ChannelError;
use serde::Deserialize;
use serde_json::json;

/// One `getUpdates` result entry.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TelegramUpdate {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<TelegramMessage>,
}

/// The `message` field of a Telegram update. Only the fields this adapter
/// needs are modeled; unknown fields are ignored by serde's default
/// (non-`deny_unknown_fields`) behavior.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TelegramMessage {
    #[allow(dead_code)]
    pub message_id: i64,
    /// Unix timestamp (seconds) the message was sent.
    pub date: i64,
    pub chat: TelegramChat,
    /// Channel posts and some service messages omit the sender.
    #[serde(default)]
    pub from: Option<TelegramUser>,
    /// Absent for non-text messages (media, stickers, etc. — out of scope
    /// for v1, ADR-056 "Out of scope (v1)").
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TelegramChat {
    pub id: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TelegramUser {
    pub id: i64,
}

#[derive(Debug, Deserialize)]
struct TelegramApiResponse<T> {
    ok: bool,
    #[serde(default)]
    result: Option<T>,
    #[serde(default)]
    error_code: Option<u16>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    parameters: Option<TelegramResponseParameters>,
}

#[derive(Debug, Deserialize)]
struct TelegramResponseParameters {
    #[serde(default)]
    retry_after: Option<u32>,
}

/// Classify a failed Bot API call by HTTP status, falling back to the
/// response body's `error_code` when it parses. `response` is `None` when
/// the body was empty, non-JSON, or otherwise failed to deserialize (a 4xx
/// status with an HTML error page from an intermediate proxy, for example);
/// the HTTP status alone is still authoritative for the transient/permanent
/// split in that case, so a client error never falls back to the transient
/// default just because its body could not be parsed as Bot API JSON.
fn telegram_api_error<T>(
    method: &str,
    status: reqwest::StatusCode,
    response: Option<&TelegramApiResponse<T>>,
) -> ChannelError {
    let error_code = response.and_then(|r| r.error_code);
    let description = response.and_then(|r| r.description.as_deref());
    let code = error_code.unwrap_or(status.as_u16());
    let message = format!(
        "{method} failed: status={status}, error_code={error_code:?}, description={description:?}"
    );
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS || code == 429 {
        if let Some(retry_after) = response
            .and_then(|r| r.parameters.as_ref())
            .and_then(|p| p.retry_after)
        {
            return ChannelError::RateLimited {
                message,
                retry_after: std::time::Duration::from_secs(u64::from(retry_after)),
            };
        }
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS || code == 408 || code == 429 || code >= 500
    {
        ChannelError::Transport(message)
    } else if (400..500).contains(&code) {
        ChannelError::PermanentTransport(message)
    } else {
        ChannelError::Transport(message)
    }
}

/// Injectable seam for the two Bot API methods this adapter uses.
#[async_trait]
pub(crate) trait TelegramConnector: Send + Sync {
    async fn send_message(&self, chat_id: i64, text: &str) -> Result<(), ChannelError>;
    async fn get_updates(&self, offset: Option<i64>) -> Result<Vec<TelegramUpdate>, ChannelError>;
}

/// A [`TelegramConnector`] implementation shared through an `Arc`, so tests
/// can retain an observable handle to a mock connector after handing a copy
/// to [`crate::channel::TelegramChannel`] (which owns a `Box<dyn
/// TelegramConnector>`).
#[async_trait]
impl<T: TelegramConnector + ?Sized> TelegramConnector for std::sync::Arc<T> {
    async fn send_message(&self, chat_id: i64, text: &str) -> Result<(), ChannelError> {
        (**self).send_message(chat_id, text).await
    }

    async fn get_updates(&self, offset: Option<i64>) -> Result<Vec<TelegramUpdate>, ChannelError> {
        (**self).get_updates(offset).await
    }
}

/// Default Bot API host. Overridable only in tests via
/// [`LiveTelegramConnector::with_base_url`], so production always targets the
/// real Telegram host.
const DEFAULT_API_BASE: &str = "https://api.telegram.org";

/// `getUpdates` long-poll timeout, in seconds, sent to the Bot API (ADR-056
/// Amendment 2026-07-05 requires long polling, not short polling).
const LONG_POLL_TIMEOUT_SECS: i64 = 25;

/// HTTP client request timeout. Must exceed [`LONG_POLL_TIMEOUT_SECS`] so a
/// `getUpdates` call that legitimately blocks server-side for the full
/// long-poll window is not cut off by the client itself.
const HTTP_CLIENT_TIMEOUT_SECS: u64 = 35;

/// Live Bot API connector: plain HTTPS POST + JSON, reusing the workspace
/// `reqwest` client (rustls-tls + json), same as `khive-channel-email`'s
/// OAuth token fetch. No Telegram SDK.
pub(crate) struct LiveTelegramConnector {
    http: reqwest::Client,
    bot_token: String,
    api_base: String,
}

impl LiveTelegramConnector {
    pub fn new(bot_token: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(HTTP_CLIENT_TIMEOUT_SECS))
            .build()
            .expect("reqwest client with static, valid config must build");
        Self {
            http,
            bot_token,
            api_base: DEFAULT_API_BASE.to_string(),
        }
    }

    /// Same as [`Self::new`] but pointed at a caller-supplied base URL
    /// instead of the real Bot API host. Test-only seam so connector error
    /// paths can be exercised against a local socket without ever reaching
    /// `api.telegram.org`.
    #[cfg(test)]
    fn with_base_url(bot_token: String, api_base: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(HTTP_CLIENT_TIMEOUT_SECS))
            .build()
            .expect("reqwest client with static, valid config must build");
        Self {
            http,
            bot_token,
            api_base,
        }
    }

    fn api_url(&self, method: &str) -> String {
        format!("{}/bot{}/{method}", self.api_base, self.bot_token)
    }
}

#[async_trait]
impl TelegramConnector for LiveTelegramConnector {
    async fn send_message(&self, chat_id: i64, text: &str) -> Result<(), ChannelError> {
        let resp = self
            .http
            .post(self.api_url("sendMessage"))
            .json(&json!({ "chat_id": chat_id, "text": text }))
            .send()
            .await
            .map_err(|e| {
                ChannelError::Transport(format!("sendMessage request failed: {}", e.without_url()))
            })?;

        let status = resp.status();
        let body = resp.text().await.map_err(|e| {
            ChannelError::Transport(format!(
                "sendMessage: failed to read response body: {}",
                e.without_url()
            ))
        })?;

        if !status.is_success() {
            let parsed: Option<TelegramApiResponse<serde_json::Value>> =
                serde_json::from_str(&body).ok();
            return Err(telegram_api_error("sendMessage", status, parsed.as_ref()));
        }

        let parsed: TelegramApiResponse<serde_json::Value> =
            serde_json::from_str(&body).map_err(|e| {
                ChannelError::Transport(format!("sendMessage: malformed response: {e}"))
            })?;

        if !parsed.ok {
            return Err(telegram_api_error("sendMessage", status, Some(&parsed)));
        }
        Ok(())
    }

    async fn get_updates(&self, offset: Option<i64>) -> Result<Vec<TelegramUpdate>, ChannelError> {
        let mut body = json!({ "timeout": LONG_POLL_TIMEOUT_SECS });
        if let Some(off) = offset {
            body["offset"] = json!(off);
        }

        let resp = self
            .http
            .post(self.api_url("getUpdates"))
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                ChannelError::Transport(format!("getUpdates request failed: {}", e.without_url()))
            })?;

        let status = resp.status();
        let body = resp.text().await.map_err(|e| {
            ChannelError::Transport(format!(
                "getUpdates: failed to read response body: {}",
                e.without_url()
            ))
        })?;

        if !status.is_success() {
            let parsed: Option<TelegramApiResponse<serde_json::Value>> =
                serde_json::from_str(&body).ok();
            return Err(telegram_api_error("getUpdates", status, parsed.as_ref()));
        }

        let parsed: TelegramApiResponse<Vec<TelegramUpdate>> = serde_json::from_str(&body)
            .map_err(|e| ChannelError::Transport(format!("getUpdates: malformed response: {e}")))?;

        if !parsed.ok {
            return Err(telegram_api_error("getUpdates", status, Some(&parsed)));
        }
        Ok(parsed.result.unwrap_or_default())
    }
}

#[cfg(test)]
#[path = "connector/tests.rs"]
mod tests;

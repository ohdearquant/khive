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
    /// Absent for non-text messages (media, stickers, etc. — out of scope
    /// for v1, ADR-056 "Out of scope (v1)").
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TelegramChat {
    pub id: i64,
}

#[derive(Debug, Deserialize)]
struct TelegramApiResponse<T> {
    ok: bool,
    #[serde(default)]
    result: Option<T>,
    #[serde(default)]
    description: Option<String>,
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

/// Live Bot API connector: plain HTTPS POST + JSON, reusing the workspace
/// `reqwest` client (rustls-tls + json), same as `khive-channel-email`'s
/// OAuth token fetch. No Telegram SDK.
pub(crate) struct LiveTelegramConnector {
    http: reqwest::Client,
    bot_token: String,
}

impl LiveTelegramConnector {
    pub fn new(bot_token: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            bot_token,
        }
    }

    fn api_url(&self, method: &str) -> String {
        format!("https://api.telegram.org/bot{}/{method}", self.bot_token)
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
            .map_err(|e| ChannelError::Transport(format!("sendMessage request failed: {e}")))?;

        let status = resp.status();
        let parsed: TelegramApiResponse<serde_json::Value> = resp.json().await.map_err(|e| {
            ChannelError::Transport(format!("sendMessage: malformed response: {e}"))
        })?;

        if !status.is_success() || !parsed.ok {
            return Err(ChannelError::Transport(format!(
                "sendMessage failed: status={status}, description={:?}",
                parsed.description
            )));
        }
        Ok(())
    }

    async fn get_updates(&self, offset: Option<i64>) -> Result<Vec<TelegramUpdate>, ChannelError> {
        let mut body = json!({ "timeout": 0 });
        if let Some(off) = offset {
            body["offset"] = json!(off);
        }

        let resp = self
            .http
            .post(self.api_url("getUpdates"))
            .json(&body)
            .send()
            .await
            .map_err(|e| ChannelError::Transport(format!("getUpdates request failed: {e}")))?;

        let status = resp.status();
        let parsed: TelegramApiResponse<Vec<TelegramUpdate>> = resp
            .json()
            .await
            .map_err(|e| ChannelError::Transport(format!("getUpdates: malformed response: {e}")))?;

        if !status.is_success() || !parsed.ok {
            return Err(ChannelError::Transport(format!(
                "getUpdates failed: status={status}, description={:?}",
                parsed.description
            )));
        }
        Ok(parsed.result.unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_url_embeds_token_and_method() {
        let connector = LiveTelegramConnector::new("123:ABC".to_string());
        assert_eq!(
            connector.api_url("sendMessage"),
            "https://api.telegram.org/bot123:ABC/sendMessage"
        );
    }

    #[test]
    fn telegram_update_deserializes_text_message() {
        let raw = json!({
            "update_id": 42,
            "message": {
                "message_id": 7,
                "date": 1_700_000_000,
                "chat": { "id": 555 },
                "text": "hello"
            }
        });
        let update: TelegramUpdate = serde_json::from_value(raw).unwrap();
        assert_eq!(update.update_id, 42);
        let message = update.message.unwrap();
        assert_eq!(message.chat.id, 555);
        assert_eq!(message.text.as_deref(), Some("hello"));
    }

    #[test]
    fn telegram_update_without_message_deserializes() {
        let raw = json!({ "update_id": 43 });
        let update: TelegramUpdate = serde_json::from_value(raw).unwrap();
        assert_eq!(update.update_id, 43);
        assert!(update.message.is_none());
    }

    #[test]
    fn telegram_message_without_text_deserializes_none() {
        let raw = json!({
            "message_id": 8,
            "date": 1_700_000_000,
            "chat": { "id": 555 }
        });
        let message: TelegramMessage = serde_json::from_value(raw).unwrap();
        assert!(message.text.is_none());
    }

    #[test]
    fn api_response_ok_false_carries_description() {
        let raw = json!({ "ok": false, "description": "Unauthorized" });
        let parsed: TelegramApiResponse<serde_json::Value> = serde_json::from_value(raw).unwrap();
        assert!(!parsed.ok);
        assert_eq!(parsed.description.as_deref(), Some("Unauthorized"));
    }
}
